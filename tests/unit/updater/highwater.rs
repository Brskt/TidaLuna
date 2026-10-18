//! Tests for `src/updater/highwater.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn load_absent_is_zero() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(load(dir.path()), "0.0.0");
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

#[test]
fn corrupt_mark_loads_as_zero() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(mark_path(dir.path()), b"{ not json").unwrap();
    assert_eq!(load(dir.path()), "0.0.0");
}

#[test]
fn non_semver_version_loads_as_zero() {
    // Valid JSON but a garbage version must not stick and block updates.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        mark_path(dir.path()),
        br#"{"max_version":"garbage","recorded_at":123}"#,
    )
    .unwrap();
    assert_eq!(load(dir.path()), "0.0.0");
}

#[test]
fn non_semver_mark_is_repaired_on_next_record() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        mark_path(dir.path()),
        br#"{"max_version":"garbage","recorded_at":123}"#,
    )
    .unwrap();
    record(dir.path(), "0.0.8-alpha");
    assert_eq!(load(dir.path()), "0.0.8-alpha");
}
