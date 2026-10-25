//! Tests for `src/updater/highwater.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn load_without_a_usable_mark_is_zero() {
    let cases: [(&str, Option<&[u8]>); 3] = [
        ("absent", None),
        ("not json", Some(b"{ not json")),
        // Inside valid JSON, it must not stick and block updates.
        (
            "garbage version",
            Some(br#"{"max_version":"garbage","recorded_at":123}"#),
        ),
    ];
    for (what, mark) in cases {
        let dir = tempfile::tempdir().unwrap();
        if let Some(bytes) = mark {
            std::fs::write(mark_path(dir.path()), bytes).unwrap();
        }
        assert_eq!(load(dir.path()), "0.0.0", "{what}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn recording_a_mark_flushes_its_directory_after_the_file() {
    use crate::platform::syscall_trace::{Event, traced};

    let dir = tempfile::tempdir().unwrap();
    let canonical = dir.path().canonicalize().unwrap();

    let ((), events) = traced(|| record(dir.path(), "0.0.9-alpha"));

    assert_eq!(
        events.last(),
        Some(&Event::Fsync {
            path: canonical,
            directory: true
        }),
        "{events:?}"
    );
}

#[test]
fn record_is_monotonic() {
    let dir = tempfile::tempdir().unwrap();
    record(dir.path(), "0.0.9-alpha");
    record(dir.path(), "0.0.8-alpha"); // lower -> ignored
    assert_eq!(load(dir.path()), "0.0.9-alpha");
    record(dir.path(), "0.0.10-alpha"); // higher -> updates
    assert_eq!(load(dir.path()), "0.0.10-alpha");
}
