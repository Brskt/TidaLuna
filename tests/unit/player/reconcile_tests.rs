//! Tests for `src/player/mod.rs`, attached to it by `#[path]`.

use super::is_same_active_track;

fn committed(canonical_id: &str, format: &str) -> (String, String) {
    (canonical_id.to_string(), format.to_string())
}

#[test]
fn same_canonical_id_and_format_is_idempotent() {
    // The caller strips the query before calling, so both ids are canonical.
    // (Production passes canonical_track_id("…?sig=2") == "…/abc".)
    let cur = committed("https://cdn/tracks/abc", "flac");
    assert!(is_same_active_track(
        Some(&cur),
        "https://cdn/tracks/abc",
        "flac"
    ));
}

#[test]
fn different_format_rebuilds() {
    let cur = committed("https://cdn/tracks/abc", "flac");
    assert!(!is_same_active_track(
        Some(&cur),
        "https://cdn/tracks/abc",
        "aac"
    ));
}

#[test]
fn different_track_rebuilds() {
    let cur = committed("https://cdn/tracks/abc", "flac");
    assert!(!is_same_active_track(
        Some(&cur),
        "https://cdn/tracks/xyz",
        "flac"
    ));
}

#[test]
fn no_committed_track_rebuilds() {
    assert!(!is_same_active_track(
        None,
        "https://cdn/tracks/abc",
        "flac"
    ));
}
