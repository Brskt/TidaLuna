//! Tests for the Linux half of `src/platform/app_lock.rs`, attached by `#[path]`.

use std::ffi::CString;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::HandOff;
use super::linux_impl::*;

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
        } => panic!("the holder has no channel: {reason}"),
        Claim::Held(_) => panic!("the first launch found the lock taken"),
        Claim::Unguarded(reason) => panic!("the guard stood down: {reason}"),
    }
}

#[test]
fn a_second_launch_hands_its_link_to_the_first_through_the_private_channel() {
    let data = tempfile::tempdir().expect("a data directory");
    let runtime = runtime_dir();
    let (_lock, channel) = holder(data.path(), runtime.path());
    enable_peer_credentials(&channel.socket).expect("enable credential passing");

    let Claim::Held(Some(addr)) = claim(data.path(), Some(runtime.path().to_owned()), own_uid())
    else {
        panic!("the second launch did not find the first holding the lock");
    };
    assert_eq!(hand_over(&addr, Some("tidal://album/1")), HandOff::Landed);

    let mut buf = [0u8; MAX_SIGNAL];
    let (n, peer) = recv_with_sender(&channel.socket, &mut buf).expect("receive");
    assert_eq!(&buf[..n], b"tidal://album/1");
    assert_eq!(peer, Some(own_uid()));
    assert!(
        channel.path.starts_with(runtime.path()),
        "a private runtime directory carries the channel"
    );
}

// A crash is exercised in a child process: the test binary runs itself on `child_holder`,
// which does nothing unless the parent names the directories it holds the lock for.

const CHILD_DATA: &str = "TIDALUNAR_APP_LOCK_TEST_DATA";
const CHILD_RUNTIME: &str = "TIDALUNAR_APP_LOCK_TEST_RUNTIME";
const CHILD_TEST: &str = "platform::app_lock::tests::child_holder";

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

#[test]
fn a_runtime_directory_others_can_enter_is_not_used() {
    let data = tempfile::tempdir().expect("a data directory");
    let runtime = runtime_dir();
    std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o755))
        .expect("open it up");

    let (_lock, channel) = holder(data.path(), runtime.path());

    assert!(channel.path.starts_with(data.path().join(FALLBACK_DIR)));
    assert_eq!(
        check_dir(
            &data.path().join(FALLBACK_DIR),
            own_uid(),
            CHANNEL_DIR_FORBIDDEN
        ),
        Ok(()),
        "the fallback directory is private"
    );
}

#[test]
fn a_data_directory_moved_behind_a_symlink_is_still_locked() {
    // Its owner moved it to another disk and left a link: the lock must hold on
    // the directory the link leads to, whichever path a launch takes.
    let real = tempfile::tempdir().expect("the moved data directory");
    let links = tempfile::tempdir().expect("where the link lives");
    let link = links.path().join("tidalunar");
    std::os::unix::fs::symlink(real.path(), &link).expect("the link");
    let runtime = runtime_dir();

    let (_lock, _channel) = holder(&link, runtime.path());
    assert!(matches!(
        claim(real.path(), Some(runtime.path().to_owned()), own_uid()),
        Claim::Held(_)
    ));
}

#[test]
fn the_channel_socket_carries_its_mode() {
    let data = tempfile::tempdir().expect("a data directory");
    let runtime = runtime_dir();
    let (_lock, channel) = holder(data.path(), runtime.path());

    let meta = std::fs::symlink_metadata(&channel.path).expect("the socket file");
    assert!(meta.file_type().is_socket(), "the channel is not a socket");
    let mode = meta.permissions().mode() & 0o7777;
    assert_eq!(
        mode, CHANNEL_MODE,
        "the socket is {mode:o}, not {CHANNEL_MODE:o}"
    );
}

#[test]
fn a_directory_is_checked_for_what_it_is_and_who_owns_it() {
    let dir = runtime_dir();
    let own = own_uid();
    assert_eq!(check_dir(dir.path(), own, 0o077), Ok(()));
    // Pretending to be another uid stands in for another account's directory.
    assert_eq!(
        check_dir(dir.path(), own.wrapping_add(1), 0o077),
        Err(DirRefusal::NotOurs(own))
    );

    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750))
        .expect("let the group in");
    assert_eq!(
        check_dir(dir.path(), own, 0o077),
        Err(DirRefusal::OpenToOthers(0o750))
    );

    // A channel directory may not be a link, where the data directory may.
    let links = tempfile::tempdir().expect("where the link lives");
    let link = links.path().join("link");
    std::os::unix::fs::symlink(dir.path(), &link).expect("a symlink to it");
    assert_eq!(check_dir(&link, own, 0), Err(DirRefusal::NotADirectory));
    assert_eq!(check_data_dir(&link, own), Ok(()));
}

fn host() -> String {
    String::from_utf8_lossy(&hostname().expect("this host's name")).into_owned()
}

/// CEF's singleton lock as a running instance leaves it in the data directory.
fn singleton_lock(data: &Path, target: &str) {
    std::fs::create_dir_all(data.join("cef")).expect("the CEF directory");
    std::os::unix::fs::symlink(target, data.join("cef").join("SingletonLock"))
        .expect("the singleton lock");
}

#[test]
fn a_legacy_name_held_by_nobody_we_know_does_not_stop_the_start() {
    // Holding the abstract name used to be holding the lock; now it stops nothing.
    let data = tempfile::tempdir().expect("a data directory");
    let runtime = runtime_dir();
    let _taken = UnixDatagram::bind_addr(&legacy_addr(data.path()).expect("the legacy name"))
        .expect("take the legacy name first");

    let (_lock, _channel) = holder(data.path(), runtime.path());
    assert!(matches!(
        legacy(data.path(), own_uid()),
        Legacy::Unavailable
    ));
}

#[test]
fn an_earlier_build_running_on_the_data_directory_is_handed_the_launch() {
    let data = tempfile::tempdir().expect("a data directory");
    let addr = legacy_addr(data.path()).expect("the legacy name");
    let earlier = UnixDatagram::bind_addr(&addr).expect("the earlier build's socket");
    // This test process stands in for the earlier build: alive, ours, and run
    // from the executable `current_exe` names.
    singleton_lock(data.path(), &format!("{}-{}", host(), std::process::id()));

    let Legacy::EarlierBuild(found) = legacy(data.path(), own_uid()) else {
        panic!("the earlier build was not recognised");
    };
    assert_eq!(hand_over(&found, Some("tidal://album/1")), HandOff::Landed);
    let mut buf = [0u8; MAX_SIGNAL];
    let n = earlier.recv(&mut buf).expect("receive");
    assert_eq!(&buf[..n], b"tidal://album/1");
}

#[test]
fn a_singleton_lock_left_by_a_dead_process_is_not_an_earlier_build() {
    let data = tempfile::tempdir().expect("a data directory");
    let _taken = UnixDatagram::bind_addr(&legacy_addr(data.path()).expect("the legacy name"))
        .expect("take the legacy name first");
    // No process can answer to a pid past the kernel's ceiling.
    singleton_lock(data.path(), &format!("{}-{}", host(), u32::MAX));

    assert!(matches!(
        legacy(data.path(), own_uid()),
        Legacy::Unavailable
    ));
}

#[test]
fn chromium_singleton_lock_names_its_host_and_pid() {
    // A made-up host with a dash of its own; only the last dash splits off the pid.
    use std::hash::{BuildHasher, Hasher};
    let host = format!(
        "pc-{:08x}",
        std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish() as u32
    );
    let target = format!("{host}-4242");
    assert_eq!(parse_singleton_lock(&target), Some((host.as_str(), 4242)));
    assert_eq!(parse_singleton_lock("host-"), None);
    assert_eq!(parse_singleton_lock("-42"), None);
    assert_eq!(parse_singleton_lock("nodash"), None);
}

/// A receiver on an abstract address unique to this test.
fn listening(tag: &str) -> (SocketAddr, UnixDatagram) {
    let addr =
        SocketAddr::from_abstract_name(format!("tidalunar-test-{tag}-{}", std::process::id()))
            .expect("an abstract address");
    let socket = UnixDatagram::bind_addr(&addr).expect("bind the receiver");
    (addr, socket)
}

/// A uid made up per run, never root and never the last one: `+ 1` is always another.
fn made_up_uid() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let raw = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish() as u32;
    raw.clamp(1, u32::MAX - 1)
}

/// An abstract socket has no permission model and its address hashes a path that is no secret: any
/// local user can reach it. Only the kernel naming our own uid makes a datagram ours. No
/// credential (also what a failed credential passing looks like) leaves nobody to attribute it to.
#[test]
fn a_datagram_is_ours_only_when_the_kernel_names_our_user() {
    let own = made_up_uid();
    for (peer, expected) in [
        (Some(own), Sender::Ours),
        (Some(0), Sender::Foreign(0)),
        (Some(own + 1), Sender::Foreign(own + 1)),
        (None, Sender::Unattributed),
    ] {
        assert_eq!(classify(peer, own), expected, "peer {peer:?}");
    }
}

/// One socket carries both meanings: a link that cannot be sent must not take the focus signal
/// with it. An over-long link is refused before the send rather than truncated on arrival (a cut
/// URL reads as a refused route at the far end).
#[test]
fn a_launch_without_a_deliverable_link_still_asks_for_focus() {
    let long = format!("tidal://album/{}", "9".repeat(MAX_SIGNAL));
    for (tag, link, outcome) in [
        ("toolong", Some(long.as_str()), HandOff::TooLong),
        ("focus", None, HandOff::Landed),
    ] {
        let (addr, socket) = listening(tag);

        assert_eq!(hand_over(&addr, link), outcome, "{tag}");

        let mut buf = [0u8; MAX_SIGNAL];
        let n = socket.recv(&mut buf).expect("receive");
        assert_eq!(
            &buf[..n],
            FOCUS_SIGNAL.as_bytes(),
            "{tag}: the window must come forward"
        );
    }
}

#[test]
fn an_address_nobody_holds_is_named_rather_than_swallowed() {
    // The abstract name auto-reclaims when the owner dies, making this the real
    // shape of a survivor that exited between our failed bind and this send.
    let addr =
        SocketAddr::from_abstract_name(format!("tidalunar-test-gone-{}", std::process::id()))
            .expect("an abstract address");

    assert_eq!(
        hand_over(&addr, Some("tidal://album/1")),
        HandOff::NoListener
    );
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
        .recv_timeout(Duration::from_secs(3))
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
