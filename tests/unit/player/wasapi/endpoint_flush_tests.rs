//! Tests for `src/player/wasapi.rs`, attached to it by `#[path]`.
//!
//! `Reset()` reaches the driver and has been measured between 1.1s and 3.4s on a
//! Focusrite endpoint; the render thread must only pay it when the endpoint really holds
//! frames. These pin that rule down as a truth table rather than as a list of the paths
//! that happen to lead here today.

use super::*;

fn ctx_with(client_started: bool, endpoint_dirty: bool) -> RenderContext {
    let mut ctx = RenderContext::new();
    ctx.client_started = client_started;
    ctx.endpoint_dirty = endpoint_dirty;
    ctx
}

#[test]
fn only_a_stopped_and_dirty_endpoint_owes_a_flush() {
    assert!(ctx_with(false, true).owes_flush());
    // A running clock empties the endpoint one period at a time.
    assert!(!ctx_with(true, true).owes_flush());
    // Nothing ever reached this endpoint.
    assert!(!ctx_with(false, false).owes_flush());
    assert!(!ctx_with(true, false).owes_flush());
}

#[test]
fn seek_does_not_stop_a_running_clock() {
    let mut ctx = ctx_with(true, true);
    ctx.current_stream_id = Some(7);
    ctx.pcm_sample_rate = 44100;
    let (event_tx, _event_rx) = mpsc::channel();

    ctx.handle_reset_for_seek(&event_tx, 7, 1, 30.0);

    // Stopping it here is what cost 1.2-2.3s per seek: the flush is not worth the clock.
    assert!(ctx.client_started);
    assert!(!ctx.owes_flush());
}

#[test]
fn seek_rebases_the_position_and_rearms_the_cushion() {
    let mut ctx = ctx_with(true, true);
    ctx.current_stream_id = Some(7);
    ctx.pcm_sample_rate = 44100;
    ctx.frames_played = 999;
    let (event_tx, event_rx) = mpsc::channel();

    ctx.handle_reset_for_seek(&event_tx, 7, 3, 2.0);

    assert_eq!(ctx.frames_played, 88200);
    assert!(ctx.pending_start);
    assert!(!ctx.stream_ended);
    assert!(matches!(
        event_rx.try_recv(),
        Ok(ExclusiveEvent::SeekSettled {
            stream_id: 7,
            gen_id: 3,
            refused: false,
            ..
        })
    ));
}

#[test]
fn a_seek_for_a_superseded_stream_changes_nothing() {
    let mut ctx = ctx_with(true, true);
    ctx.current_stream_id = Some(7);
    ctx.pcm_sample_rate = 44100;
    ctx.frames_played = 999;
    let (event_tx, event_rx) = mpsc::channel();

    ctx.handle_reset_for_seek(&event_tx, 8, 1, 2.0);

    assert_eq!(ctx.frames_played, 999);
    assert!(!ctx.pending_start);
    assert!(event_rx.try_recv().is_err());
}

#[test]
fn releasing_the_device_leaves_no_flush_owed() {
    let mut ctx = ctx_with(false, true);
    assert!(ctx.owes_flush());

    ctx.release_device();

    // The client is dropped, and the frames it held go with it.
    assert!(!ctx.owes_flush());
    assert!(!ctx.endpoint_dirty);
}
