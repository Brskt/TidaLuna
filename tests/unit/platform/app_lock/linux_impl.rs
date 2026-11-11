//! Tests for `src/platform/app_lock/linux_impl.rs`, attached to it by `#[path]`.

use std::ffi::CString;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::*;

/// A private runtime directory, as logind makes `/run/user/<uid>`.
fn runtime_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a runtime directory");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
        .expect("make it private");
    dir
}

fn holder(data: &Path, runtime: &Path) -> (std::fs::File, Channel) {
    match claim(data, Some(runtime.to_owned()), own_uid()) {
        Claim::Holder {
            lock,
            channel: Ok(channel),
        } => (lock, channel),
        Claim::Holder {
            channel: Err(reason),
            ..
        } => panic!("the holder has no channel: {reason:?}"),
        Claim::Held(_) => panic!("the first launch found the lock taken"),
        Claim::Unguarded(reason) => panic!("the guard stood down: {reason}"),
    }
}

// A crash is exercised in a child process: the test binary runs itself on `child_holder`,
// which does nothing unless the parent names the directories it holds the lock for.

const CHILD_DATA: &str = "TIDALUNAR_APP_LOCK_TEST_DATA";
const CHILD_RUNTIME: &str = "TIDALUNAR_APP_LOCK_TEST_RUNTIME";
const CHILD_TEST: &str = "platform::app_lock::linux_impl::tests::child_holder";

#[test]
fn child_holder() {
    let (Ok(data), Ok(runtime)) = (std::env::var(CHILD_DATA), std::env::var(CHILD_RUNTIME)) else {
        return;
    };
    let (_lock, channel) = holder(Path::new(&data), Path::new(&runtime));
    writeln!(std::io::stdout(), "HOLDING {}", channel.path.display()).expect("tell the parent");
    // Held until the parent kills this process.
    loop {
        std::thread::park();
    }
}

#[test]
fn a_holder_that_died_leaves_nothing_in_the_way() {
    // A crash releases the flock with the process, and leaves the socket file. The holder is a
    // child: a lock closed in this process stays held by any fork a parallel test is making.
    let data = tempfile::tempdir().expect("a data directory");
    let runtime = runtime_dir();
    let mut child = Command::new(std::env::current_exe().expect("the test binary has a path"))
        .args(["--exact", CHILD_TEST, "--nocapture"])
        .env(CHILD_DATA, data.path())
        .env(CHILD_RUNTIME, runtime.path())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the test binary starts");
    let stale = BufReader::new(child.stdout.take().expect("a piped stdout"))
        .lines()
        .find_map(|line| Some(PathBuf::from(line.ok()?.strip_prefix("HOLDING ")?)))
        .expect("the child took the lock");
    child.kill().expect("kill the holder");
    child.wait().expect("reap the holder");
    assert!(
        stale.exists(),
        "the dead holder's socket file is still there"
    );

    let (_lock, channel) = holder(data.path(), runtime.path());
    assert_eq!(channel.path, stale);
    assert!(matches!(
        claim(data.path(), Some(runtime.path().to_owned()), own_uid()),
        Claim::Held(Some(_))
    ));
}

/// An inotify queue on `dirs`: the kernel fills it in the order things happen.
fn watch(dirs: &[&Path]) -> File {
    // SAFETY: plain syscalls on arguments we own; the descriptor is wrapped at once.
    let queue = unsafe {
        let fd = libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK);
        assert!(fd >= 0, "inotify_init1 failed");
        File::from_raw_fd(fd)
    };
    for dir in dirs {
        let path = CString::new(dir.as_os_str().as_bytes()).expect("a path without NUL");
        // SAFETY: `path` is a NUL-terminated string that outlives the call.
        let id = unsafe {
            libc::inotify_add_watch(queue.as_raw_fd(), path.as_ptr(), libc::IN_CLOSE_WRITE)
        };
        assert!(id >= 0, "inotify_add_watch failed");
    }
    queue
}

/// What the queue holds, as `(mask, file name)`; empties it.
fn drain(queue: &mut File) -> Vec<(u32, String)> {
    let mut events = Vec::new();
    let mut buf = [0u8; 4096];
    // The queue is non-blocking: an empty one reads as an error.
    while let Ok(n @ 1..) = queue.read(&mut buf) {
        let mut at = 0;
        // `struct inotify_event`: wd, mask, cookie, len (4 bytes each), then `len` name bytes.
        while at + 16 <= n {
            let word = |i: usize| u32::from_ne_bytes(buf[at + i..at + i + 4].try_into().unwrap());
            let (mask, len) = (word(4), word(12) as usize);
            let name = &buf[at + 16..at + 16 + len];
            let name = name.split(|&b| b == 0).next().unwrap_or_default();
            events.push((mask, String::from_utf8_lossy(name).into_owned()));
            at += 16 + len;
        }
    }
    events
}

/// The lock outlives the service by the whole teardown. A launch that came in during it handed
/// its link to a channel nobody read any more and left; it waits for the lock now, and is the
/// holder when the old one is gone, with its link still to show.
#[test]
fn a_launch_that_finds_the_holder_closing_outwaits_it_and_takes_over() {
    let data = tempfile::tempdir().expect("a data directory");
    let runtime = runtime_dir();
    let first = acquire_in(data.path(), Some(runtime.path().to_owned()), None)
        .expect("the first launch holds the lock");
    first._guard.as_ref().expect("a guard").closer.close();

    let (done, reached) = std::sync::mpsc::channel();
    let (data_path, runtime_path) = (data.path().to_owned(), runtime.path().to_owned());
    let second = std::thread::spawn(move || {
        let launched = acquire_in(&data_path, Some(runtime_path), Some("tidal://album/1"));
        done.send(()).expect("the test is waiting");
        launched
    });

    assert!(
        reached.recv_timeout(Duration::from_millis(300)).is_err(),
        "the second launch left while the first still held the lock"
    );
    drop(first);
    reached
        .recv_timeout(crate::state::HANG_GUARD)
        .expect("the second launch never took over");
    assert!(
        second.join().expect("the second launch panicked").is_some(),
        "the second launch gave its link up instead of holding the lock"
    );
}

/// The holder takes the lock and binds its socket a moment apart, and a launch that arrives
/// between finds the lock taken and no socket. It looks again for a short while, and the link
/// lands instead of being reported lost.
#[test]
fn a_launch_that_beats_the_holders_bind_still_lands_its_link() {
    let data = tempfile::tempdir().expect("a data directory");
    let runtime = runtime_dir();
    let (_lock, channel) = holder(data.path(), runtime.path());
    std::fs::remove_file(&channel.path).expect("take the socket away, as before the bind");

    let mut opened = watch(&[data.path()]);
    drain(&mut opened);
    let (data_path, runtime_path) = (data.path().to_owned(), runtime.path().to_owned());
    let second = std::thread::spawn(move || {
        acquire_in(&data_path, Some(runtime_path), Some("tidal://album/1"))
    });
    // Each try of the second launch opens and closes the lock file. Its second try is proof the
    // first found no socket and chose to look again; the bind that follows is late by its own
    // account, not by a clock. A launch that gave up never makes one, and the link is lost.
    let mut tries = 0;
    let give_up = Instant::now() + Duration::from_secs(5);
    while tries < 2 && Instant::now() < give_up {
        tries += drain(&mut opened)
            .iter()
            .filter(|(mask, name)| name == "instance.lock" && mask & libc::IN_CLOSE_WRITE != 0)
            .count();
        std::thread::sleep(Duration::from_millis(1));
    }
    let late = UnixDatagram::bind(&channel.path).expect("the holder binds at last");
    late.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("a bounded read");

    let mut buf = [0u8; MAX_SIGNAL];
    let n = late
        .recv(&mut buf)
        .expect("the link never reached the late socket");
    assert_eq!(&buf[..n], b"tidal://album/1");
    assert!(
        second.join().expect("the second launch panicked").is_none(),
        "a link that landed leaves nothing for the second launch to hold"
    );
}

/// A signal that cuts a receive short is no death. The reader retries, delivers the next link
/// and logs nothing at the close.
#[test]
fn a_reader_interrupted_by_a_signal_still_delivers_the_next_link() {
    static CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    const LINK: &[u8] = b"tidal://album/eintr-survivor";
    fn interrupted_then_a_link_then_the_close(
        _: &UnixDatagram,
        buf: &mut [u8],
    ) -> std::io::Result<(usize, Option<u32>)> {
        match CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0..=2 => Err(std::io::ErrorKind::Interrupted.into()),
            3 => {
                buf[..LINK.len()].copy_from_slice(LINK);
                Ok((LINK.len(), Some(own_uid())))
            }
            _ => Ok((0, None)),
        }
    }
    let (socket, _) = UnixDatagram::pair().expect("a socket pair");
    let closing = std::sync::atomic::AtomicBool::new(true);
    let fate = Fate::new("channel");

    read_until_closed(
        &socket,
        &closing,
        &fate,
        interrupted_then_a_link_then_the_close,
    );

    assert_eq!(CALLS.load(std::sync::atomic::Ordering::SeqCst), 5);
    let delivered = crate::ui::deep_link::take_pending().expect("the link was delivered");
    assert!(delivered.to_string().contains("eintr-survivor"));
    fate.report();
    assert!(
        crate::logging::take_lines().is_empty(),
        "the reader reported a death"
    );
}
