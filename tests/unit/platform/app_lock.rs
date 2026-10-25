//! Tests for the Linux half of `src/platform/app_lock.rs`, attached by `#[path]`.

use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::Path;

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

#[test]
fn a_holder_that_died_leaves_nothing_in_the_way() {
    // A crash releases the flock with the process, and leaves the socket file.
    let data = tempfile::tempdir().expect("a data directory");
    let runtime = runtime_dir();
    let (lock, channel) = holder(data.path(), runtime.path());
    let stale = channel.path.clone();
    std::mem::forget(channel);
    drop(lock);
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
