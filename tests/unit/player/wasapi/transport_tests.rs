//! Tests for `src/player/wasapi.rs`, attached to it by `#[path]`.
//!
//! What a `Play` means to the render loop, stated once for each state it can reach. The drain
//! serving `Playing` handles a whole burst of commands before it re-reads its own state, so
//! "we are playing" is a premise no handler may carry from one command to the next.

use super::*;
use crate::player::PlaybackState;

fn ctx_on(state: RenderState, stream_id: u32) -> RenderContext {
    let mut ctx = RenderContext::new();
    ctx.current_stream_id = Some(stream_id);
    ctx.state = state;
    ctx
}

#[test]
fn a_play_for_the_live_stream_while_playing_changes_nothing() {
    let mut ctx = ctx_on(RenderState::Playing, 7);
    let (event_tx, event_rx) = mpsc::channel();

    ctx.apply_play(&event_tx, 7);

    assert!(matches!(ctx.state, RenderState::Playing));
    assert!(ctx.pending_transport.is_none());
    assert!(
        event_rx.try_recv().is_err(),
        "a redundant Play announced a transition the render never made"
    );
}

#[test]
fn a_play_for_the_live_stream_resumes_a_parked_render() {
    // Reached two ways, and the second is the defect this pins: the parked arm gets one
    // command at a time, but the playing drain handled `Pause{7}` and the `Play{7}` answering
    // it in a single `try_recv` sweep. There the id matched and the arm read that as a no-op,
    // so the render parked while `is_playing` stayed true upstream: silence, and only a
    // second Play could undo it.
    let mut ctx = ctx_on(RenderState::Paused, 7);
    let (event_tx, event_rx) = mpsc::channel();

    ctx.apply_play(&event_tx, 7);

    assert!(matches!(ctx.state, RenderState::Playing));
    assert!(matches!(
        event_rx.try_recv(),
        Ok(ExclusiveEvent::StateChange(PlaybackState::Active))
    ));
}

#[test]
fn a_play_for_a_stream_not_yet_adopted_is_latched_never_applied() {
    // Its `StartStream` lands only after the decoder's probe. Resuming here would start the
    // context still armed for the previous track.
    let mut ctx = ctx_on(RenderState::Paused, 7);
    let (event_tx, event_rx) = mpsc::channel();

    ctx.apply_play(&event_tx, 8);

    assert_eq!(ctx.pending_transport, Some((8, true)));
    assert!(matches!(ctx.state, RenderState::Paused));
    assert!(
        event_rx.try_recv().is_err(),
        "a latched Play announced a resume the render never made"
    );
}
