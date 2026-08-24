//! Tests for `src/player/thread/playback.rs`, attached to it by `#[path]`.

use super::decode_failure_needs_settle;
#[cfg(target_os = "windows")]
use super::seek_ack_is_current;
#[cfg(target_os = "windows")]
use super::verdict_names_current_stream;

#[cfg(target_os = "windows")]
#[test]
fn an_ack_from_a_replaced_decoder_settles_nothing() {
    // The stream half: a track change mints a new decoder, and its predecessor's answer
    // must not land on it.
    assert!(!seek_ack_is_current(Some(8), 3, 7, 3));
}

#[cfg(target_os = "windows")]
#[test]
fn a_sibling_seeks_ack_settles_nothing() {
    // The generation half: same live decoder means `stream_id` matches; only the generation
    // separates the seek being waited on from an earlier one still answering. Without
    // this the older answer clears the newer seek's pin.
    assert!(!seek_ack_is_current(Some(7), 4, 7, 3));
}

#[cfg(target_os = "windows")]
#[test]
fn only_the_awaited_seeks_own_ack_settles_it() {
    assert!(seek_ack_is_current(Some(7), 3, 7, 3));
}

#[cfg(target_os = "windows")]
#[test]
fn an_ack_before_any_stream_settles_nothing() {
    assert!(!seek_ack_is_current(None, 3, 7, 3));
}

#[cfg(target_os = "windows")]
#[test]
fn a_superseded_streams_refusal_condemns_no_track() {
    // The defect: a rate-locked device takes seconds to refuse a candidate, and a track
    // change inside that window left the arriving track marked shared-only for a format
    // the device had never been asked about.
    assert!(!verdict_names_current_stream(Some(2), Some(1)));
}

#[cfg(target_os = "windows")]
#[test]
fn the_live_streams_refusal_condemns_its_own_track() {
    assert!(verdict_names_current_stream(Some(2), Some(2)));
}

#[cfg(target_os = "windows")]
#[test]
fn a_verdict_that_names_no_stream_condemns_no_track() {
    // The ASIO build path can refuse before any stream is adopted. Two unidentified sides
    // are not the same stream, so `Option == Option` is the wrong comparison here: it would
    // read None == None as a match and mark whatever happened to be playing.
    assert!(!verdict_names_current_stream(None, None));
    assert!(!verdict_names_current_stream(Some(2), None));
    assert!(!verdict_names_current_stream(None, Some(2)));
}

#[test]
fn init_time_error_settles() {
    // No trailing Finished: pending_complete stays false, nothing else emits
    // a terminal state.
    assert!(decode_failure_needs_settle(false, 0));
    assert!(decode_failure_needs_settle(false, 4096));
}

#[test]
fn mid_stream_error_before_first_sample_settles() {
    // Finished arrived but the drain gate (played > 0) can never pass.
    assert!(decode_failure_needs_settle(true, 0));
}

#[test]
fn mid_stream_error_with_audio_drains_to_completed() {
    // The existing drain path owns this case (Completed after ring drain).
    assert!(!decode_failure_needs_settle(true, 4096));
}
