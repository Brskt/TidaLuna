use std::fs::{File, OpenOptions, TryLockError};
use std::hash::Hasher;
use std::io::ErrorKind;
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{
    AppLock, Attempt, CLOSE_WAIT_MS, Focus, HANDOFF_BUDGET_MS, HandOff, NO_LISTENER_GRACE_MS,
    Unguarded, Unserved, join_within, post_focus,
};

/// Who sent a datagram, as the kernel reports it rather than as the payload
/// claims.
///
/// The channel's own socket sits in a directory no other account can enter,
/// but the legacy abstract name is still bound, and `unix(7)` is explicit that
/// an abstract socket has no permission model: every process in the same
/// network namespace can reach it, whoever owns it.
#[derive(Debug, PartialEq)]
pub(super) enum Sender {
    /// Credentials the kernel vouched for, naming this process's own user.
    Ours,
    /// Another local user. Nothing here is theirs to drive.
    Foreign(u32),
    /// No credential arrived, leaving nobody to attribute it to.
    Unattributed,
}

/// Kept off the receive path so that a foreign sender can be exercised
/// without one. A child forked in a test carries the test's own uid, and one
/// account cannot produce another's.
pub(super) fn classify(peer: Option<u32>, own: u32) -> Sender {
    match peer {
        Some(uid) if uid == own => Sender::Ours,
        Some(uid) => Sender::Foreign(uid),
        None => Sender::Unattributed,
    }
}

pub(super) fn own_uid() -> u32 {
    // SAFETY: reads the calling process's real uid and cannot fail.
    unsafe { libc::getuid() }
}

/// Asks the kernel to attach the sender's credentials to every datagram that
/// arrives afterwards. Without it `recvmsg` carries no control message and
/// there is nothing to check.
pub(super) fn enable_peer_credentials(socket: &UnixDatagram) -> std::io::Result<()> {
    let on: libc::c_int = 1;
    // SAFETY: the option value is a `c_int` and the length passed is its own
    // size, which is the shape `SO_PASSCRED` expects. The descriptor is owned
    // by `socket`, which outlives the call.
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PASSCRED,
            std::ptr::from_ref(&on).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// One datagram, plus the uid the kernel attributes it to.
///
/// The credential travels as a control message, and `std` still gates its
/// ancillary-data support behind nightly; the chain is walked here. The
/// union is what aligns that buffer for a `cmsghdr`. An array of bytes
/// promises no alignment, and its `align` arm exists for its type rather
/// than its value.
pub(super) fn recv_with_sender(
    socket: &UnixDatagram,
    buf: &mut [u8],
) -> std::io::Result<(usize, Option<u32>)> {
    #[repr(C)]
    union ControlBuffer {
        align: libc::cmsghdr,
        bytes: [u8; 64],
    }

    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut control = ControlBuffer { bytes: [0; 64] };
    // SAFETY: `msghdr` is a C struct, and the fields left alone below are the
    // ones the kernel expects to read as zero.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = std::ptr::from_mut(&mut control).cast();
    msg.msg_controllen = std::mem::size_of::<ControlBuffer>() as _;

    // SAFETY: `msg` names the two buffers above, both of which outlive this
    // call, and the descriptor belongs to `socket`.
    let received = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut msg, 0) };
    if received < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut peer = None;
    // SAFETY: the receive succeeded, so `msg`'s control fields describe what
    // the kernel wrote into our own buffer, and the two macros only ever
    // yield a pointer inside it, aligned for a header by the union above.
    // Each read is unaligned so that neither claims more than that.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            let header = std::ptr::read_unaligned(cmsg);
            if header.cmsg_level == libc::SOL_SOCKET && header.cmsg_type == libc::SCM_CREDENTIALS {
                let cred = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::ucred>());
                peer = Some(cred.uid);
                break;
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }

    Ok((received as usize, peer))
}

/// What a second launch sends when it carries no URL. One socket carries both
/// meanings here, unlike Windows where the deep link has a channel of its own
/// and the focus signal keeps its data-less event.
pub(super) const FOCUS_SIGNAL: &str = "focus";

/// Sized for a deep link rather than the 5-byte focus word. A datagram longer
/// than this is truncated by the kernel, and the truncated remainder fails the
/// URL rule rather than navigating somewhere unintended.
pub(super) const MAX_SIGNAL: usize = 4096;

/// How often a launch waiting for the lock tries it again: `std` has no lock
/// wait with a deadline, and a try is one syscall.
const LOCK_POLL_MS: u64 = 10;

/// The lock is a file in the data directory it guards, so every launch that
/// would share the database takes the same lock, whatever its environment.
const LOCK_FILE: &str = "instance.lock";

/// Group and other bits a channel directory must not carry: its socket is
/// reachable by whoever can enter it.
pub(super) const CHANNEL_DIR_FORBIDDEN: u32 = 0o077;

/// The channel's directory inside the data directory, when the runtime one
/// is not private.
pub(super) const FALLBACK_DIR: &str = "run";

/// Owner read-write, plus the sticky bit the XDG spec names for a file of the
/// runtime directory that must survive its periodic clean-up.
pub(super) const CHANNEL_MODE: u32 = 0o1600;

fn data_dir_hash(data_dir: &Path) -> u64 {
    let mut hasher = fnv::FnvHasher::default();
    hasher.write(data_dir.to_string_lossy().as_bytes());
    hasher.finish()
}

/// Keyed on the data directory, like the lock: two data directories of one
/// account never share a channel.
pub(super) fn channel_name(data_dir: &Path) -> String {
    format!("tidalunar-{:016x}.sock", data_dir_hash(data_dir))
}

/// The name earlier builds bind. No longer the lock (any account can hold an
/// abstract name), it stays bound for an earlier build launched now to find.
pub(super) fn legacy_addr(data_dir: &Path) -> std::io::Result<SocketAddr> {
    SocketAddr::from_abstract_name(format!("tidalunar-app-{:016x}", data_dir_hash(data_dir)))
}

/// Why a directory cannot carry the lock or the channel.
#[derive(Debug, PartialEq)]
pub(super) enum DirRefusal {
    Missing,
    /// A file, or a symlink: what gets checked must be what gets used.
    NotADirectory,
    /// Owned by another uid, kept for the log line.
    NotOurs(u32),
    /// The mode, carrying a bit `forbidden` names.
    OpenToOthers(u32),
}

/// A real directory, owned by `own`, with none of the `forbidden` bits.
///
/// Checked on the path, not a descriptor: replacing the directory between
/// this check and the bind needs write access to its parent, which is ours
/// for `FALLBACK_DIR` and root's for `/run/user`.
pub(super) fn check_dir(path: &Path, own: u32, forbidden: u32) -> Result<(), DirRefusal> {
    check_meta(std::fs::symlink_metadata(path), own, forbidden)
}

/// The data directory, owned by `own`, its mode unchecked: the database lives
/// there too, and a lock is no more private than it. It may be a symlink, as
/// when its owner moved it to another disk; what must be ours is where it
/// leads, and the link sits in a parent only its owner can change.
pub(super) fn check_data_dir(path: &Path, own: u32) -> Result<(), DirRefusal> {
    check_meta(std::fs::metadata(path), own, 0)
}

fn check_meta(
    meta: std::io::Result<std::fs::Metadata>,
    own: u32,
    forbidden: u32,
) -> Result<(), DirRefusal> {
    let meta = meta.map_err(|_| DirRefusal::Missing)?;
    if !meta.file_type().is_dir() {
        return Err(DirRefusal::NotADirectory);
    }
    if meta.uid() != own {
        return Err(DirRefusal::NotOurs(meta.uid()));
    }
    let mode = meta.mode() & 0o7777;
    if mode & forbidden != 0 {
        return Err(DirRefusal::OpenToOthers(mode));
    }
    Ok(())
}

/// Where the channel's socket lives: the session's runtime directory when it
/// is as private as the XDG spec promises, else our own `FALLBACK_DIR` in the
/// data directory. `None` when neither is private.
pub(super) fn channel_dir(data_dir: &Path, runtime: Option<PathBuf>, own: u32) -> Option<PathBuf> {
    if let Some(dir) = runtime.filter(|dir| dir.is_absolute())
        && check_dir(&dir, own, CHANNEL_DIR_FORBIDDEN).is_ok()
    {
        return Some(dir);
    }
    let run = data_dir.join(FALLBACK_DIR);
    match std::fs::DirBuilder::new().mode(0o700).create(&run) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }
    check_dir(&run, own, CHANNEL_DIR_FORBIDDEN)
        .is_ok()
        .then_some(run)
}

/// The bound end of the channel. Its file is never unlinked on exit: only
/// `bind` removes one, under the lock. A successor's socket is out of reach
/// of the leaving holder.
pub(super) struct Channel {
    pub(super) socket: UnixDatagram,
    pub(super) path: PathBuf,
}

impl Channel {
    /// Only the lock holder binds, so a socket file already there belongs to a
    /// holder that is gone, and unlinking it cannot take a live one. A channel
    /// exists only with `CHANNEL_MODE` set.
    fn bind(path: PathBuf) -> std::io::Result<Self> {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let channel = Self {
            socket: UnixDatagram::bind(&path)?,
            path,
        };
        // Still under the lock: a refused mode leaves no socket behind.
        if let Err(e) =
            std::fs::set_permissions(&channel.path, std::fs::Permissions::from_mode(CHANNEL_MODE))
        {
            let _ = std::fs::remove_file(&channel.path);
            return Err(e);
        }
        Ok(channel)
    }
}

/// What a launch finds for its data directory.
pub(super) enum Claim {
    /// This process holds the lock. `channel` is the reason when none could
    /// be bound: the lock still holds, but neither focus nor links can arrive.
    Holder {
        lock: File,
        channel: Result<Channel, Unserved>,
    },
    /// Another process of this account holds it, listening here; `None` when
    /// no private directory could name its channel.
    Held(Option<SocketAddr>),
    /// The guard cannot stand, and says why.
    Unguarded(Unguarded),
}

/// Takes the data directory's lock, or names the channel of whoever holds it.
pub(super) fn claim(data_dir: &Path, runtime: Option<PathBuf>, own: u32) -> Claim {
    if let Err(e) = std::fs::create_dir_all(data_dir) {
        return Claim::Unguarded(Unguarded::DataDir(e));
    }
    if let Err(refusal) = check_data_dir(data_dir, own) {
        return Claim::Unguarded(Unguarded::DataDirRefused(refusal));
    }
    let lock = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(data_dir.join(LOCK_FILE))
    {
        Ok(lock) => lock,
        Err(e) => return Claim::Unguarded(Unguarded::LockFile(e)),
    };
    let path = channel_dir(data_dir, runtime, own).map(|dir| dir.join(channel_name(data_dir)));
    match lock.try_lock() {
        Ok(()) => Claim::Holder {
            lock,
            channel: match path {
                Some(path) => Channel::bind(path).map_err(Unserved::Bind),
                None => Err(Unserved::NoChannelDir),
            },
        },
        Err(TryLockError::WouldBlock) => {
            Claim::Held(path.and_then(|path| SocketAddr::from_pathname(path).ok()))
        }
        Err(TryLockError::Error(e)) => Claim::Unguarded(Unguarded::Lock(e)),
    }
}

/// `(hostname, pid)` from Chromium's `SingletonLock` symlink target,
/// `<hostname>-<pid>`. The hostname may itself hold dashes.
pub(super) fn parse_singleton_lock(target: &str) -> Option<(&str, u32)> {
    let (host, pid) = target.rsplit_once('-')?;
    if host.is_empty() {
        return None;
    }
    Some((host, pid.parse().ok()?))
}

pub(super) fn hostname() -> Option<Vec<u8>> {
    let mut name = [0u8; 256];
    // SAFETY: the buffer is ours and its length is passed; gethostname writes
    // at most that many bytes.
    if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } != 0 {
        return None;
    }
    let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    Some(name[..len].to_vec())
}

/// An earlier build of ours runs on this data directory: CEF's `SingletonLock`
/// names a live process of this host and account, with our executable name.
/// Such a build takes no file lock; the symlink is its one attributable trace,
/// present once CEF is up. The path is its layout, whatever later builds use.
fn earlier_build_running(data_dir: &Path, own: u32) -> bool {
    let Ok(target) = std::fs::read_link(data_dir.join("cef").join("SingletonLock")) else {
        return false;
    };
    let Some((host, pid)) = target.to_str().and_then(parse_singleton_lock) else {
        return false;
    };
    if hostname().as_deref() != Some(host.as_bytes()) {
        return false;
    }
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let ours = std::fs::metadata(&proc).is_ok_and(|meta| meta.uid() == own);
    let exe = std::fs::read_link(proc.join("exe")).ok();
    let own_exe = std::env::current_exe().ok();
    ours && exe.is_some()
        && exe.as_deref().and_then(Path::file_name) == own_exe.as_deref().and_then(Path::file_name)
}

/// Asks for the credentials of every datagram, then listens. A failed ask is
/// logged, not fatal: every datagram then arrives unattributed and is refused.
/// Gives back what closing needs: a handle on the socket and the listener.
fn listen(
    socket: &UnixDatagram,
    closing: &Arc<AtomicBool>,
    fate: &Arc<Fate>,
) -> std::io::Result<(UnixDatagram, JoinHandle<()>)> {
    if let Err(e) = enable_peer_credentials(socket) {
        crate::vprintln!("[LOCK]   no peer credentials ({e}); signals will be refused");
    }
    let for_listener = socket.try_clone()?;
    let for_closing = socket.try_clone()?;
    let listener = spawn_focus_listener(for_listener, Arc::clone(closing), Arc::clone(fate))?;
    Ok((for_closing, listener))
}

/// Why a reader left without being closed.
enum Cause {
    Failed(std::io::Error),
    Panicked,
}

enum Standing {
    /// The session log is not ours yet, and the reader still runs or has died unheard.
    Waiting(Option<Cause>),
    /// `report_channels` has run: a later death is said where it happens.
    Reported,
}

/// What became of one socket's reader. The mutex makes recording and saying
/// atomic: a death is said once, by `report_channels` if it came first and by
/// the reader if it came after.
pub(super) struct Fate {
    socket: &'static str,
    standing: Mutex<Standing>,
}

impl Fate {
    pub(super) fn new(socket: &'static str) -> Self {
        Self {
            socket,
            standing: Mutex::new(Standing::Waiting(None)),
        }
    }

    fn died(&self, cause: Cause) {
        let mut standing = self.standing.lock().unwrap_or_else(|e| e.into_inner());
        match &mut *standing {
            Standing::Waiting(slot) => *slot = Some(cause),
            Standing::Reported => {
                drop(standing);
                self.say(&cause);
            }
        }
    }

    pub(super) fn report(&self) {
        let before = std::mem::replace(
            &mut *self.standing.lock().unwrap_or_else(|e| e.into_inner()),
            Standing::Reported,
        );
        if let Standing::Waiting(Some(cause)) = before {
            self.say(&cause);
        }
    }

    /// Ungated: a reader that stopped leaves a bound socket nobody reads.
    fn say(&self, cause: &Cause) {
        match cause {
            Cause::Failed(e) => crate::verr!(
                "[LOCK]   the {} reader stopped ({e}); what reaches it is lost",
                self.socket
            ),
            Cause::Panicked => crate::verr!(
                "[LOCK]   the {} reader panicked; what reaches it is lost",
                self.socket
            ),
        }
    }
}

/// What turns the channel away: the flag the listeners read, a handle on each
/// socket to shut, and the listeners to wait for.
pub(super) struct Closer {
    closing: Arc<AtomicBool>,
    sockets: Vec<UnixDatagram>,
    listeners: Mutex<Vec<JoinHandle<()>>>,
}

impl Closer {
    /// Stops reading new datagrams and delivers what already arrived. A sender
    /// after this gets `EPIPE`, the file staying where it is (only `bind`
    /// removes it), and the pending link is final once this returns.
    pub(super) fn close(&self) {
        // Held throughout: a second caller waits for the first to be done.
        let mut listeners = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        // Before the shutdown, which the listeners can only tell from an empty
        // datagram by this flag.
        self.closing.store(true, Ordering::Release);
        for socket in &self.sockets {
            if let Err(e) = socket.shutdown(Shutdown::Read) {
                crate::verr!(
                    "[LOCK]   the channel could not be closed ({e}); links may still arrive"
                );
            }
        }
        for listener in listeners.drain(..) {
            if !join_within(listener, Duration::from_millis(CLOSE_WAIT_MS)) {
                crate::verr!("[LOCK]   a channel listener did not stop; a link may be left unread");
            }
        }
    }
}

/// The closer of the guard `acquire_or_signal` handed out, for the sites that
/// have no handle on the lock, which is a local of `main`.
static CLOSER: OnceLock<Arc<Closer>> = OnceLock::new();

pub(super) fn close_channel() {
    if let Some(closer) = CLOSER.get() {
        closer.close();
    }
}

/// What the holder of the lock makes of the legacy name.
pub(super) enum Legacy {
    /// Bound: an earlier build launched now hands over to this instance.
    Bound(UnixDatagram),
    /// An earlier build of ours holds it and runs on this data directory;
    /// this launch hands over to it instead of starting beside it.
    EarlierBuild(SocketAddr),
    /// Held by a process nobody can attribute, possibly another account's,
    /// or no name at all. This instance holds the lock that matters.
    Unavailable,
}

pub(super) fn legacy(data_dir: &Path, own: u32) -> Legacy {
    let Ok(addr) = legacy_addr(data_dir) else {
        return Legacy::Unavailable;
    };
    match UnixDatagram::bind_addr(&addr) {
        Ok(socket) => Legacy::Bound(socket),
        Err(e) if e.kind() == ErrorKind::AddrInUse && earlier_build_running(data_dir, own) => {
            Legacy::EarlierBuild(addr)
        }
        Err(_) => Legacy::Unavailable,
    }
}

/// Held for the process lifetime.
pub(crate) struct Guard {
    _lock: File,
    _channel: Option<Channel>,
    _legacy: Option<UnixDatagram>,
    pub(super) closer: Arc<Closer>,
    fates: Vec<Arc<Fate>>,
}

impl Guard {
    /// Says the deaths that came before the session log was ours, and the
    /// readers say their own from here on.
    pub(super) fn report_deaths(&self) {
        for fate in &self.fates {
            fate.report();
        }
    }
}

pub(super) fn acquire_or_signal(url: Option<&str>) -> Option<AppLock> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let lock = acquire_in(&crate::state::cache_data_dir(), runtime, url)?;
    if let Some(guard) = &lock._guard {
        let _ = CLOSER.set(Arc::clone(&guard.closer));
    }
    Some(lock)
}

/// `acquire_or_signal` on the given directories (a test needs no global). Same
/// hand-over and takeover as the Windows twin, under one shared deadline.
pub(super) fn acquire_in(
    data_dir: &Path,
    runtime: Option<PathBuf>,
    url: Option<&str>,
) -> Option<AppLock> {
    let own = own_uid();
    let deadline = Instant::now() + Duration::from_millis(HANDOFF_BUDGET_MS);
    let mut unbound_since: Option<Instant> = None;
    let mut waited = false;
    loop {
        match claim(data_dir, runtime.clone(), own) {
            Claim::Holder { lock, channel } => {
                if waited {
                    crate::vprintln!("[LOCK]   The previous instance has left; taking over");
                }
                return hold(data_dir, own, lock, channel, url);
            }
            Claim::Held(addr) => {
                let outcome = match &addr {
                    Some(addr) => hand_over(addr, url),
                    None => Attempt::unasked(HandOff::NoListener),
                };
                // A leaving holder is waited for to the deadline. No socket is
                // not told apart from a holder that has not bound yet: it only
                // gets the grace that race needs.
                let patience = match (&outcome.link, &addr) {
                    (HandOff::Closing, _) => Some(deadline),
                    (HandOff::NoListener, Some(_)) => {
                        let since = *unbound_since.get_or_insert_with(Instant::now);
                        let grace = Duration::from_millis(NO_LISTENER_GRACE_MS);
                        Some((since + grace).min(deadline))
                    }
                    _ => None,
                };
                if patience.is_some_and(|until| Instant::now() < until) {
                    waited = true;
                    std::thread::sleep(Duration::from_millis(LOCK_POLL_MS));
                    continue;
                }
                report_hand_off(url, &outcome);
                return None;
            }
            Claim::Unguarded(cause) => {
                return Some(AppLock {
                    _guard: None,
                    unserved: vec![Unserved::Unguarded(cause)],
                });
            }
        }
    }
}

/// The first instance: its channel and the legacy name listen, and `closer`
/// is what turns them away later.
fn hold(
    data_dir: &Path,
    own: u32,
    lock: File,
    channel: Result<Channel, Unserved>,
    url: Option<&str>,
) -> Option<AppLock> {
    let legacy_socket = match legacy(data_dir, own) {
        Legacy::Bound(socket) => Some(socket),
        Legacy::EarlierBuild(addr) => {
            report_hand_off(url, &hand_over(&addr, url));
            return None;
        }
        Legacy::Unavailable => None,
    };
    let closing = Arc::new(AtomicBool::new(false));
    let (mut sockets, mut listeners, mut unserved) = (Vec::new(), Vec::new(), Vec::new());
    let mut fates = Vec::new();
    let channel = match channel {
        Ok(channel) => Some(channel),
        Err(unbound) => {
            unserved.push(unbound);
            None
        }
    };
    let mut serve =
        |socket: &UnixDatagram, name: &'static str, failed: fn(std::io::Error) -> Unserved| {
            let fate = Arc::new(Fate::new(name));
            match listen(socket, &closing, &fate) {
                Ok((end, listener)) => {
                    sockets.push(end);
                    listeners.push(listener);
                    fates.push(fate);
                    true
                }
                Err(e) => {
                    unserved.push(failed(e));
                    false
                }
            }
        };
    // The channel without a reader is dropped: the flock is the lock, and a launch
    // then finds nobody to hand to instead of a holder that never reads.
    let channel = channel.filter(|c| serve(&c.socket, "channel", Unserved::Channel));
    // The legacy name stays bound (earlier builds take its bind as their lock).
    if let Some(socket) = &legacy_socket {
        serve(socket, "legacy", Unserved::Legacy);
    }
    Some(AppLock {
        _guard: Some(Guard {
            _lock: lock,
            _channel: channel,
            _legacy: legacy_socket,
            closer: Arc::new(Closer {
                closing,
                sockets,
                listeners: Mutex::new(listeners),
            }),
            fates,
        }),
        unserved,
    })
}

fn report_hand_off(url: Option<&str>, outcome: &Attempt) {
    match (url, &outcome.link) {
        (Some(_), HandOff::Landed) => {
            crate::vprintln!("[LOCK]   Handed a deep link to the running instance");
        }
        // Ungated like the rest: the wait ended with the old instance still
        // there, and the link it was waiting to carry goes nowhere.
        (Some(_), HandOff::Closing) => crate::verr!(
            "[LOCK]   deep link lost: the running instance was still closing after the wait"
        ),
        (None, HandOff::Closing) => crate::verr!(
            "[LOCK]   not started: the running instance was still closing after the wait"
        ),
        (Some(_), _) => outcome.report_lost(),
        (None, HandOff::Landed) => {
            crate::vprintln!("[LOCK]   Another instance is running; {}", outcome.focus);
        }
        (None, lost) => crate::verr!(
            "[LOCK]   could not reach the running instance ({})",
            lost.reason()
        ),
    }
}

/// Hands over whatever this launch carried, falling back to the bare focus
/// word so a click still brings the window forward.
///
/// One socket carries both meanings here, unlike Windows where the focus
/// signal keeps an event of its own. Failing to open or connect it loses the
/// focus too, and that is the one case nothing here can rescue. It is
/// reported instead.
pub(super) fn hand_over(addr: &SocketAddr, url: Option<&str>) -> Attempt {
    let Ok(client) = UnixDatagram::unbound() else {
        return Attempt::unasked(HandOff::Failed);
    };
    if client.connect_addr(addr).is_err() {
        return Attempt::unasked(HandOff::NoListener);
    }
    let Some(url) = url else {
        let link = send_or_fail(&client, FOCUS_SIGNAL);
        let focus = if link == HandOff::Landed {
            Focus::Asked
        } else {
            Focus::NotAsked
        };
        return Attempt { link, focus };
    };
    // Refused here rather than truncated by the kernel on arrival, since a
    // cut URL fails the route rule at the far end and reads as a refused
    // link instead of an oversized one.
    if url.len() > MAX_SIGNAL {
        return Attempt {
            link: HandOff::TooLong,
            focus: ask_focus(&client),
        };
    }
    match client.send(url.as_bytes()) {
        Ok(_) => return Attempt::unasked(HandOff::Landed),
        // No focus word either: it would be turned away the same way.
        Err(e) if closed_by_peer(&e) => return Attempt::unasked(HandOff::Closing),
        Err(_) => {}
    }
    // The link did not go; the window still should.
    Attempt {
        link: HandOff::Failed,
        focus: ask_focus(&client),
    }
}

fn ask_focus(client: &UnixDatagram) -> Focus {
    match client.send(FOCUS_SIGNAL.as_bytes()) {
        Ok(_) => Focus::Asked,
        Err(_) => Focus::NotAsked,
    }
}

fn send_or_fail(client: &UnixDatagram, word: &str) -> HandOff {
    match client.send(word.as_bytes()) {
        Ok(_) => HandOff::Landed,
        Err(e) if closed_by_peer(&e) => HandOff::Closing,
        Err(_) => HandOff::Failed,
    }
}

/// What a send gets once the receiver shut its reading end (`EPIPE`) or closed
/// its socket after we connected (`ECONNREFUSED`). Only a send says so: a
/// connect to a socket nobody serves is a holder not bound yet, or never bound.
fn closed_by_peer(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::BrokenPipe | ErrorKind::ConnectionRefused
    )
}

fn spawn_focus_listener(
    socket: UnixDatagram,
    closing: Arc<AtomicBool>,
    fate: Arc<Fate>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("app-focus-listener".to_owned())
        .spawn(move || read_until_closed(&socket, &closing, &fate, recv_with_sender))
}

/// One datagram and its sender's uid; a test passes one that a signal cuts short.
pub(super) type Recv = fn(&UnixDatagram, &mut [u8]) -> std::io::Result<(usize, Option<u32>)>;

/// Armed for the whole read loop, disarmed only by a close. Any other way out (a
/// failed receive, an unwind) is recorded; the socket stays bound and open.
struct Exit<'a> {
    fate: &'a Fate,
    cause: Option<Cause>,
}

impl Drop for Exit<'_> {
    fn drop(&mut self) {
        if let Some(cause) = self.cause.take() {
            self.fate.died(cause);
        }
    }
}

pub(super) fn read_until_closed(
    socket: &UnixDatagram,
    closing: &AtomicBool,
    fate: &Fate,
    recv: Recv,
) {
    let own = own_uid();
    let mut buf = [0u8; MAX_SIGNAL];
    let mut exit = Exit {
        fate,
        cause: Some(Cause::Panicked),
    };
    loop {
        // The length matters now that the payload varies (a receive that
        // ignored it would read stale bytes from the previous datagram).
        let (n, peer) = match recv(socket, &mut buf) {
            Ok(received) => received,
            // A signal handler installed without `SA_RESTART` ends the wait early.
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                exit.cause = Some(Cause::Failed(e));
                return;
            }
        };
        // A shut socket reads as empty once its queue is drained, as an
        // empty datagram does; only the flag tells them apart.
        if n == 0 && closing.load(Ordering::Acquire) {
            exit.cause = None;
            return;
        }
        match classify(peer, own) {
            Sender::Ours => match std::str::from_utf8(&buf[..n]) {
                Ok(FOCUS_SIGNAL) => post_focus(),
                Ok(url) => crate::ui::deep_link::deliver(url),
                Err(_) => post_focus(),
            },
            Sender::Foreign(uid) => refuse(Some(uid)),
            Sender::Unattributed => refuse(None),
        }
    }
}

/// Said once per run. A local process can repeat a refused datagram as fast
/// as it likes, and a log it can fill is a second defect.
fn refuse(uid: Option<u32>) {
    static ANNOUNCED: AtomicBool = AtomicBool::new(false);
    if ANNOUNCED.swap(true, Ordering::Relaxed) {
        return;
    }
    match uid {
        Some(uid) => crate::vprintln!("[LOCK]   refused a signal from uid {uid}"),
        None => crate::vprintln!("[LOCK]   refused a signal nobody is named for"),
    }
}
