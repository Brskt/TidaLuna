//! Tests for `src/player/wasapi.rs`, attached to it by `#[path]`.
//!
//! What a `Play` means to the render loop, stated once for each state it can reach. The drain
//! serving `Playing` handles a whole burst of commands before it re-reads its own state, so
//! "we are playing" is a premise no handler may carry from one command to the next.

use super::*;
use crate::player::PlaybackState;

/// The seq of the load that opened the stream a context holds.
const OPENED_UNDER: u32 = 3;

fn ctx_on(state: RenderState, stream_id: u32) -> RenderContext {
    let mut ctx = RenderContext::new();
    ctx.current_stream_id = Some(stream_id);
    ctx.opened_under = OPENED_UNDER;
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
        Ok(ExclusiveEvent::StateChange {
            opened_under: OPENED_UNDER,
            state: PlaybackState::Active
        })
    ));
}

/// Stream 7, opened under `OPENED_UNDER`.
fn stream_seven(start_paused: bool) -> StreamStart {
    StreamStart {
        stream_id: 7,
        opened_under: OPENED_UNDER,
        sample_rate: 44_100,
        channels: 2,
        bits_per_sample: 16,
        duration_secs: 180.0,
        start_secs: 0.0,
        start_paused,
        consumed: Arc::new(AtomicU64::new(0)),
    }
}

/// What the render adopts for stream 7 from an idle context.
fn adopting(start_paused: bool) -> (RenderContext, mpsc::Receiver<ExclusiveEvent>) {
    let mut ctx = RenderContext::new();
    let (event_tx, event_rx) = mpsc::channel();
    ctx.adopt_stream(&event_tx, stream_seven(start_paused));
    (ctx, event_rx)
}

#[test]
fn an_adopted_stream_announces_its_length_then_starts_under_the_load_that_opened_it() {
    let (ctx, event_rx) = adopting(false);

    assert!(matches!(ctx.state, RenderState::Playing));
    assert_eq!(ctx.opened_under, OPENED_UNDER);
    assert!(matches!(
        event_rx.try_recv(),
        Ok(ExclusiveEvent::Duration { stream_id: 7, .. })
    ));
    assert!(matches!(
        event_rx.try_recv(),
        Ok(ExclusiveEvent::StateChange {
            opened_under: OPENED_UNDER,
            state: PlaybackState::Active
        })
    ));
}

#[test]
fn a_stream_adopted_paused_reports_the_pause_under_the_load_that_opened_it() {
    let (ctx, event_rx) = adopting(true);

    assert!(matches!(ctx.state, RenderState::Paused));
    let _length = event_rx.try_recv();
    assert!(matches!(
        event_rx.try_recv(),
        Ok(ExclusiveEvent::StateChange {
            opened_under: OPENED_UNDER,
            state: PlaybackState::Paused
        })
    ));
}

#[test]
fn a_pause_latched_before_the_adoption_starts_the_stream_paused_under_its_load() {
    let mut ctx = RenderContext::new();
    let (event_tx, event_rx) = mpsc::channel();
    ctx.pending_transport = Some((7, false));

    ctx.adopt_stream(&event_tx, stream_seven(false));

    assert!(ctx.pending_transport.is_none());
    let _length = event_rx.try_recv();
    assert!(matches!(
        event_rx.try_recv(),
        Ok(ExclusiveEvent::StateChange {
            opened_under: OPENED_UNDER,
            state: PlaybackState::Paused
        })
    ));
}

#[test]
fn a_pause_after_the_adoption_reports_the_load_the_adoption_recorded() {
    // The two halves meet here: what `adopt_stream` records is what `apply_pause` reports.
    let (mut ctx, event_rx) = adopting(false);
    while event_rx.try_recv().is_ok() {}
    let (event_tx, event_rx) = mpsc::channel();

    ctx.apply_pause(&event_tx, 7);

    assert!(matches!(
        event_rx.try_recv(),
        Ok(ExclusiveEvent::StateChange {
            opened_under: OPENED_UNDER,
            state: PlaybackState::Paused
        })
    ));
}

#[test]
fn a_pause_for_the_live_stream_reports_the_load_that_opened_it() {
    // The report can reach the player after a track change, and the seq is all that tells it
    // whose pause this is.
    let mut ctx = ctx_on(RenderState::Playing, 7);
    let (event_tx, event_rx) = mpsc::channel();

    ctx.apply_pause(&event_tx, 7);

    assert!(matches!(ctx.state, RenderState::Paused));
    assert!(matches!(
        event_rx.try_recv(),
        Ok(ExclusiveEvent::StateChange {
            opened_under: OPENED_UNDER,
            state: PlaybackState::Paused
        })
    ));
}

#[test]
fn a_pause_for_a_stream_not_yet_adopted_is_latched_and_reports_nothing() {
    let mut ctx = ctx_on(RenderState::Playing, 7);
    let (event_tx, event_rx) = mpsc::channel();

    ctx.apply_pause(&event_tx, 8);

    assert_eq!(ctx.pending_transport, Some((8, false)));
    assert!(matches!(ctx.state, RenderState::Playing));
    assert!(
        event_rx.try_recv().is_err(),
        "a latched Pause announced a pause the render never made"
    );
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
