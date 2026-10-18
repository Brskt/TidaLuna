//! Tests for `src/player/mod.rs`, attached to it by `#[path]`.

use super::is_same_active_track;

fn committed(canonical_id: &str, format: &str) -> (String, String) {
    (canonical_id.to_string(), format.to_string())
}

#[test]
fn only_the_same_track_in_the_same_format_is_kept_and_anything_else_rebuilds() {
    // The caller strips the query before calling; both ids are canonical.
    // (Production passes canonical_track_id("…?sig=2") == "…/abc".)
    let cur = committed("https://cdn/tracks/abc", "flac");
    let table = [
        (Some(&cur), "https://cdn/tracks/abc", "flac", true),
        (Some(&cur), "https://cdn/tracks/abc", "aac", false),
        (Some(&cur), "https://cdn/tracks/xyz", "flac", false),
        (None, "https://cdn/tracks/abc", "flac", false),
    ];
    for (current, id, format, same) in table {
        assert_eq!(
            is_same_active_track(current, id, format),
            same,
            "{current:?} against {id} as {format}"
        );
    }
}
