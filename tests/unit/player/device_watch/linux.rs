//! Tests for `src/player/device_watch/linux.rs`, attached to it by `#[path]`.

use super::{Reaction, react, start_at};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// One inotify event as the kernel writes it: the header, then the name padded with NULs. An
/// empty name has no padding at all.
fn event(mask: u32, name: &[u8]) -> Vec<u8> {
    let padded = if name.is_empty() { 0 } else { 16u32 };
    let mut bytes = Vec::new();
    bytes.extend(1i32.to_ne_bytes());
    bytes.extend(mask.to_ne_bytes());
    bytes.extend(0u32.to_ne_bytes());
    bytes.extend(padded.to_ne_bytes());
    let mut name = name.to_vec();
    name.resize(padded as usize, 0);
    bytes.extend(name);
    bytes
}

/// A read holds several events; a misread header, or the wrong one winning, leaves a card the
/// page never hears of.
#[test]
fn each_read_asks_the_strongest_reaction_its_events_need() {
    use libc::{IN_CREATE, IN_DELETE, IN_IGNORED, IN_ISDIR, IN_Q_OVERFLOW};
    let cases: [(Vec<u8>, Reaction, &str); 6] = [
        (
            event(IN_CREATE, b"pcmC1D0p"),
            Reaction::Ignore,
            "a PCM node is no card",
        ),
        (
            [
                event(IN_CREATE, b"pcmC1D0p"),
                event(IN_CREATE, b"controlC1"),
            ]
            .concat(),
            Reaction::Relist,
            "a card's control node after another node",
        ),
        (
            [
                event(IN_CREATE, b"controlC0"),
                event(IN_CREATE | IN_ISDIR, b"snd"),
            ]
            .concat(),
            Reaction::Rearm,
            "the directory's return after a card",
        ),
        (
            event(IN_DELETE | IN_ISDIR, b"snd"),
            Reaction::Relist,
            "the directory gone",
        ),
        (event(IN_IGNORED, b""), Reaction::Ignore, "a dead watch"),
        (event(IN_Q_OVERFLOW, b""), Reaction::Rearm, "events dropped"),
    ];
    for (events, expected, what) in cases {
        assert_eq!(react(&events), expected, "{what}");
    }
}

/// The cards in `snd`, as a re-list would find them when signalled.
fn cards(snd: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(snd)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("controlC"))
        .collect();
    names.sort();
    names
}

/// Waits for a re-list that finds exactly `expected`.
fn await_list(lists: &mpsc::Receiver<Vec<String>>, expected: &[&str]) {
    let deadline = std::time::Instant::now() + crate::state::HANG_GUARD;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match lists.recv_timeout(left) {
            Ok(list) if list == expected => return,
            Ok(_) => {}
            Err(_) => panic!("no re-list ever found {expected:?}"),
        }
    }
}

/// `/dev/snd` exists only from ALSA's first node to its last: a host that starts with no card,
/// or loses its last one, still has the next card it gets listed.
#[test]
fn a_card_is_seen_whether_its_directory_existed_or_not() {
    let dev = tempfile::tempdir().unwrap();
    let snd: PathBuf = dev.path().join("snd");
    let (tx, lists) = mpsc::channel();
    let seen = snd.clone();
    let _watch = start_at(
        dev.path(),
        std::sync::Arc::new(move || {
            let _ = tx.send(cards(&seen));
        }),
    )
    .expect("no snd directory is a host with no card yet, not a failure");

    std::fs::create_dir(&snd).unwrap();
    std::fs::File::create(snd.join("controlC0")).unwrap();
    await_list(&lists, &["controlC0"]);

    std::fs::remove_file(snd.join("controlC0")).unwrap();
    std::fs::remove_dir(&snd).unwrap();
    await_list(&lists, &[]);

    std::fs::create_dir(&snd).unwrap();
    std::fs::File::create(snd.join("controlC1")).unwrap();
    await_list(&lists, &["controlC1"]);

    // Whichever signal found controlC1 came after the directory was watched again.
    std::fs::File::create(snd.join("controlC2")).unwrap();
    await_list(&lists, &["controlC1", "controlC2"]);
}
