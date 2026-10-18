//! Tests for the load banner's staleness decision in `src/player/mod.rs`, attached by `#[path]`.

use super::announcement_is_stale;

#[test]
fn an_announcement_is_stale_only_when_it_names_another_load() {
    let table = [
        // The gapless advance: Rust commits the next track before the renderer announces it.
        // The slot still names the track that just ended, and its title must be withheld.
        (Some("261445590"), Some("261445589"), true),
        // The case that makes `same_track` the wrong gate here. A recover derives its id from the
        // retained source and gets `None` when that was never claimed, while the announced
        // metadata describes the track correctly. Reading absence as disagreement would withhold
        // a title that was right, which is worse than the defect this decision exists for.
        (None, Some("261445589"), false),
        // First load after startup: the slot is empty until the renderer's first frame. There is
        // no disagreement to report, and the banner falls back to its own Unknown placeholders.
        (Some("261445590"), None, false),
        (None, None, false),
        // Every user-initiated play: the metadata frame precedes the load, and the ids agree.
        (Some("261445589"), Some("261445589"), false),
    ];
    for (load, announced, stale) in table {
        assert_eq!(
            announcement_is_stale(load, announced),
            stale,
            "load {load:?}, announced {announced:?}"
        );
    }
}
