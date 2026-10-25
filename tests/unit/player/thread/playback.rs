//! Tests for `src/player/thread/playback.rs`, attached to it by `#[path]`.

use super::decode_failure_needs_settle;
#[cfg(target_os = "windows")]
use super::seek_ack_is_current;
use super::stream_state_seq;
#[cfg(target_os = "windows")]
use super::verdict_names_current_stream;

/// A track change mints a new decoder, and its predecessor's answer must not land on it (stream
/// half). On one live decoder only the generation separates the awaited seek from an earlier
/// one still answering, which would clear the newer seek's pin (generation half).
#[cfg(target_os = "windows")]
#[test]
fn only_the_awaited_seeks_own_ack_settles_it() {
    for (current_stream, current_gen, ack_stream, ack_gen, expected) in [
        (Some(8), 3, 7, 3, false),
        (Some(7), 4, 7, 3, false),
        (Some(7), 3, 7, 3, true),
        (None, 3, 7, 3, false),
    ] {
        assert_eq!(
            seek_ack_is_current(current_stream, current_gen, ack_stream, ack_gen),
            expected,
            "current ({current_stream:?}, {current_gen}), ack ({ack_stream}, {ack_gen})"
        );
    }
}

/// A rate-locked device takes seconds to refuse a candidate: a superseded stream's verdict
/// must not mark the track that arrived meanwhile. Two unidentified sides are not the same
/// stream (`None == None` read as a match would mark whatever happened to be playing).
#[cfg(target_os = "windows")]
#[test]
fn a_verdict_condemns_a_track_only_when_both_sides_name_the_current_stream() {
    for (current, verdict, expected) in [
        (Some(2), Some(1), false),
        (Some(2), Some(2), true),
        (None, None, false),
        (Some(2), None, false),
        (None, Some(2), false),
    ] {
        assert_eq!(
            verdict_names_current_stream(current, verdict),
            expected,
            "current {current:?}, verdict {verdict:?}"
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
