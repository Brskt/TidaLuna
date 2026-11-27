//! Tests for `src/player/thread/playback.rs`, attached to it by `#[path]`.

use super::decode_failure_needs_settle;
use super::{relays_backend_state, stream_state_seq};
use crate::player::PlaybackState::{Active, Paused, Stopped};

/// A stop's late pause ack, or an `active` that raced it, lands after `stopped` and leaves the page
/// on it. A play since the stop makes both the page's again, a pause since it its pause; a retired
/// stream's states keep relaying, its pause being the page's only NOT_PLAYING for it.
#[test]
fn a_stopped_streams_late_pause_or_start_stays_off_the_page() {
    for (state, opened_under, idle, playing, expected) in [
        (Paused, 5, Stopped, false, false),
        (Paused, 5, Stopped, true, true),
        (Active, 5, Stopped, false, false),
        (Active, 5, Stopped, true, true),
        (Paused, 5, Paused, false, true),
        (Paused, 4, Stopped, false, true),
    ] {
        assert_eq!(
            relays_backend_state(state, opened_under, 5, idle, playing),
            expected,
            "relays_backend_state({state:?}, opened under {opened_under}, idle {idle:?}, playing {playing})"
        );
    }
}

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
