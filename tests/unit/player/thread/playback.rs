//! Tests for `src/player/thread/playback.rs`, attached to it by `#[path]`.

use super::decode_failure_needs_settle;
use super::stream_state_seq;

/// A stop since the load moved the seq on and kept the stream: what it reports now belongs to
/// the stop's seq. A stream an earlier load opened keeps that load's seq, or its late pause
/// reads on the Connect receiver as the new track's own.
#[test]
fn a_bypass_state_travels_under_the_seq_of_the_load_that_opened_its_stream() {
    for (opened_under, load_seq, current_seq, expected) in
        [(5, 5, 5, 5), (5, 5, 7, 7), (4, 5, 5, 4), (4, 5, 8, 4)]
    {
        assert_eq!(
            stream_state_seq(opened_under, load_seq, current_seq),
            expected,
            "stream_state_seq({opened_under}, {load_seq}, {current_seq})"
        );
    }
}

/// Without a trailing Finished (`pending_complete` false) nothing else emits a terminal state;
/// with one, the drain gate (`played > 0`) can never pass before the first sample.
#[test]
fn a_decode_failure_settles_in_place_when_the_drain_path_cannot_take_over() {
    for (pending_complete, played_samples) in [(false, 0), (false, 4096), (true, 0)] {
        assert!(
            decode_failure_needs_settle(pending_complete, played_samples),
            "pending_complete {pending_complete}, played {played_samples}"
        );
    }
}
