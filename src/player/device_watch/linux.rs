use std::ffi::{CString, OsStr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::thread::JoinHandle;

use super::Signal;

/// ALSA, the host cpal lists from, names each card by a `controlC<n>` node in `/dev/snd`: a card
/// added or removed creates or deletes exactly one.
const CARD_NODE: &[u8] = b"controlC";
/// `/dev/snd` lives from ALSA's first node to its last: its parent is what always stays watched.
const SND: &[u8] = b"snd";

/// Watching stops when this drops: closing the pipe's write end wakes the thread.
pub(crate) struct Watch {
    wake: Option<OwnedFd>,
    thread: Option<JoinHandle<()>>,
}

pub(crate) fn start(signal: Signal) -> Result<Watch, String> {
    start_at(Path::new("/dev"), signal)
}

/// `parent` stands for `/dev`, which a test replaces with a directory of its own.
fn start_at(parent: &Path, signal: Signal) -> Result<Watch, String> {
    let c_path = |path: &Path| CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string());
    let parent_path = c_path(parent)?;
    let snd = c_path(&parent.join(OsStr::from_bytes(SND)))?;
    let os_error = |what: &str| format!("{what}: {}", std::io::Error::last_os_error());
    // SAFETY: plain syscalls; each descriptor they return is owned at once.
    let inotify = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if inotify < 0 {
        return Err(os_error("inotify"));
    }
    let inotify = unsafe { OwnedFd::from_raw_fd(inotify) };
    // SAFETY: a live descriptor and a NUL-terminated path.
    let watched = unsafe {
        libc::inotify_add_watch(
            inotify.as_raw_fd(),
            parent_path.as_ptr(),
            libc::IN_CREATE | libc::IN_DELETE,
        )
    };
    if watched < 0 {
        return Err(os_error(&parent.display().to_string()));
    }
    let watcher = Watcher {
        inotify,
        snd,
        signal,
    };
    watcher.arm_snd();
    let mut pipe = [0; 2];
    // SAFETY: `pipe` has room for the two descriptors written.
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(os_error("pipe"));
    }
    // SAFETY: both descriptors were just created and are owned nowhere else.
    let (woken, wake) = unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
    let thread = std::thread::Builder::new()
        .name("device-watch".into())
        .spawn(move || watcher.run(&woken))
        .map_err(|e| format!("thread: {e}"))?;
    Ok(Watch {
        wake: Some(wake),
        thread: Some(thread),
    })
}

struct Watcher {
    inotify: OwnedFd,
    snd: CString,
    signal: Signal,
}

impl Watcher {
    /// Watch `/dev/snd` if it is there. Absent, not a directory or unreadable is a host with no
    /// card yet: the parent's watch sees the directory come.
    fn arm_snd(&self) {
        // SAFETY: a live descriptor and a NUL-terminated path. A directory already watched only
        // hands its watch back.
        unsafe {
            libc::inotify_add_watch(
                self.inotify.as_raw_fd(),
                self.snd.as_ptr(),
                libc::IN_CREATE | libc::IN_DELETE | libc::IN_ONLYDIR,
            );
        }
    }

    fn run(&self, woken: &OwnedFd) {
        let mut fds = [
            libc::pollfd {
                fd: self.inotify.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: woken.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: `fds` holds two live descriptors for the whole call.
            if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
                if !stopped_by("poll") {
                    continue;
                }
                return;
            }
            if fds[1].revents != 0 {
                return;
            }
            if fds[0].revents & libc::POLLIN == 0 {
                continue;
            }
            // SAFETY: reads at most `buf.len()` bytes into `buf`.
            let read =
                unsafe { libc::read(self.inotify.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            let Ok(read) = usize::try_from(read) else {
                if !stopped_by("read") {
                    continue;
                }
                return;
            };
            match react(&buf[..read]) {
                Reaction::Ignore => {}
                Reaction::Relist => (self.signal)(),
                Reaction::Rearm => {
                    self.arm_snd();
                    (self.signal)();
                }
            }
        }
    }
}

/// Whether the call that just failed ends the watch; an interrupted one is retried.
fn stopped_by(call: &str) -> bool {
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::Interrupted {
        return false;
    }
    crate::verr!("[AUDIO]  Device list stops refreshing on its own: {call}: {error}");
    true
}

/// What one read of inotify events asks for.
#[derive(Debug, PartialEq, Eq)]
enum Reaction {
    Ignore,
    /// A card came or went, or the directory holding them went.
    Relist,
    /// `snd` appeared, or the queue overflowed and dropped events: watch it again, then re-list
    /// what it may already hold.
    Rearm,
}

/// The strongest reaction any event of one read asks for. A dead watch's `IN_IGNORED` names
/// nothing and asks nothing: the parent re-arms when the directory returns.
fn react(events: &[u8]) -> Reaction {
    let header = std::mem::size_of::<libc::inotify_event>();
    let mut reaction = Reaction::Ignore;
    let mut offset = 0;
    while offset + header <= events.len() {
        // SAFETY: a whole header lies at `offset`; the buffer has no alignment guarantee.
        let event: libc::inotify_event =
            unsafe { std::ptr::read_unaligned(events[offset..].as_ptr().cast()) };
        let name_start = offset + header;
        let name_end = (name_start + event.len as usize).min(events.len());
        let padded = &events[name_start..name_end];
        let name = padded.split(|&b| b == 0).next().unwrap_or_default();
        if event.mask & libc::IN_Q_OVERFLOW != 0
            || (name == SND && event.mask & libc::IN_CREATE != 0)
        {
            return Reaction::Rearm;
        }
        if name == SND || name.starts_with(CARD_NODE) {
            reaction = Reaction::Relist;
        }
        offset = name_end;
    }
    reaction
}

impl Drop for Watch {
    fn drop(&mut self) {
        drop(self.wake.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/player/device_watch/linux.rs"]
mod tests;
