//! Tests for `src/player/thread/playback.rs`, attached to it by `#[path]`.

use super::decode_failure_needs_settle;

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
