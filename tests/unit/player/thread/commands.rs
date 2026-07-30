//! Tests for `src/player/thread/commands.rs`, attached to it by `#[path]`.

use super::{PlayAction, decide_play, settle_load};

#[test]
fn play_with_live_track_resumes() {
    assert_eq!(decide_play(true, None, false), PlayAction::Resume);
    assert_eq!(decide_play(true, Some(7), true), PlayAction::Resume);
}

#[test]
fn play_while_loading_defers_to_that_generation() {
    assert_eq!(decide_play(false, Some(7), true), PlayAction::DeferTo(7));
    // a load in flight wins even when a retained source also exists
    assert_eq!(decide_play(false, Some(3), false), PlayAction::DeferTo(3));
}

#[test]
fn play_with_no_load_but_retained_source_rearms() {
    assert_eq!(decide_play(false, None, true), PlayAction::ReArm);
}

#[test]
fn play_cold_empty_is_ignored() {
    assert_eq!(decide_play(false, None, false), PlayAction::Ignore);
}

#[test]
fn settle_clears_loading_for_matching_gen() {
    assert_eq!(settle_load(Some(5), None, 5), (None, None));
}

#[test]
fn settle_ignores_a_mismatched_gen() {
    // a stale settle for an old gen must not clear a newer in-flight load
    assert_eq!(settle_load(Some(6), None, 5), (Some(6), None));
}

#[test]
fn settle_clears_a_deferred_play_waiting_on_a_failed_load() {
    // the load failed (no handle_load delivered), so the deferred play
    // tagged with that gen must not dangle
    assert_eq!(settle_load(Some(5), Some(5), 5), (None, None));
}

#[test]
fn settle_leaves_a_deferred_play_for_a_different_gen() {
    assert_eq!(settle_load(None, Some(9), 5), (None, Some(9)));
}
