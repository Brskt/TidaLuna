//! Tests for the load banner's staleness decision in `src/player/mod.rs`, attached by `#[path]`.

use super::announcement_is_stale;

#[test]
fn a_load_the_announcement_does_not_name_is_stale() {
    // The gapless advance: Rust commits the next track before the renderer announces it, so
    // the slot still names the track that just ended and its title must be withheld.
    assert!(announcement_is_stale(Some("261445590"), Some("261445589")));
}

#[test]
fn a_load_with_no_id_of_its_own_is_not_stale() {
    // The case that makes `same_track` the wrong gate here. A recover derives its id from the
    // retained source and gets `None` when that was never claimed, while the announced
    // metadata describes the track correctly. Reading absence as disagreement would withhold
    // a title that was right, which is worse than the defect this decision exists for.
    assert!(!announcement_is_stale(None, Some("261445589")));
}

#[test]
fn nothing_announced_yet_is_not_stale() {
    // First load after startup: the slot is empty until the renderer's first frame. There is
    // no disagreement to report, and the banner falls back to its own Unknown placeholders.
    assert!(!announcement_is_stale(Some("261445590"), None));
    assert!(!announcement_is_stale(None, None));
}

#[test]
fn an_announcement_naming_this_load_is_not_stale() {
    // Every user-initiated play: the metadata frame precedes the load, so the ids agree and
    // the real title is printed.
    assert!(!announcement_is_stale(Some("261445589"), Some("261445589")));
}
