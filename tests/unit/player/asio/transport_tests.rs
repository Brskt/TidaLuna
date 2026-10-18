//! Tests for `src/player/asio/host.rs`, attached to it by `#[path]`.
//!
//! The transport states the control thread reports, and the load each one names. Nothing here
//! opens a driver: `ControlCtx::new` holds none, and the transport arms below only ever reach the
//! driver through an `Option` that is empty.

use super::*;

const STREAM: u32 = 7;
/// The seq of the load that opened `STREAM`.
const OPENED_UNDER: u32 = 3;

fn control() -> ControlCtx {
    ControlCtx::new(Arc::new(AtomicU32::new(0)), None)
}

/// A control thread that has adopted `STREAM` for the load `OPENED_UNDER`, its announcements
/// already drained.
fn adopted() -> (
    ControlCtx,
    mpsc::Sender<AsioEvent>,
    mpsc::Receiver<AsioEvent>,
) {
    let mut ctx = control();
    let (event_tx, event_rx) = mpsc::channel();
    ctx.adopt_stream_identity(
        &event_tx,
        STREAM,
        OPENED_UNDER,
        44_100,
        2,
        180.0,
        0.0,
        Arc::new(AtomicU64::new(0)),
    );
    while event_rx.try_recv().is_ok() {}
    (ctx, event_tx, event_rx)
}

#[test]
fn an_adopted_stream_starts_under_the_load_that_opened_it() {
    let (mut ctx, event_tx, event_rx) = adopted();

    ctx.settle_transport(false, &event_tx);

    assert!(matches!(
        event_rx.try_recv(),
        Ok(AsioEvent::StateChange {
            opened_under: OPENED_UNDER,
            state: PlaybackState::Active
        })
    ));
}

#[test]
fn a_stream_that_starts_paused_reports_the_pause_under_its_load() {
    let (mut ctx, event_tx, event_rx) = adopted();

    ctx.settle_transport(true, &event_tx);

    assert!(matches!(
        event_rx.try_recv(),
        Ok(AsioEvent::StateChange {
            opened_under: OPENED_UNDER,
            state: PlaybackState::Paused
        })
    ));
}

#[test]
fn a_pause_for_the_live_stream_reports_the_load_that_opened_it() {
    let (mut ctx, event_tx, event_rx) = adopted();
    ctx.state = AsioState::Playing;

    let exit = ctx.dispatch(AsioCommand::Pause { stream_id: STREAM }, &event_tx);

    assert!(!exit);
    assert!(matches!(ctx.state, AsioState::Paused));
    assert!(matches!(
        event_rx.try_recv(),
        Ok(AsioEvent::StateChange {
            opened_under: OPENED_UNDER,
            state: PlaybackState::Paused
        })
    ));
}

#[test]
fn a_resume_of_the_live_stream_reports_the_load_that_opened_it() {
    let (mut ctx, event_tx, event_rx) = adopted();
    ctx.state = AsioState::Paused;
    ctx.has_buffers = true;

    let exit = ctx.dispatch(AsioCommand::Play { stream_id: STREAM }, &event_tx);

    assert!(!exit);
    assert!(matches!(ctx.state, AsioState::Playing));
    assert!(matches!(
        event_rx.try_recv(),
        Ok(AsioEvent::StateChange {
            opened_under: OPENED_UNDER,
            state: PlaybackState::Active
        })
    ));
}

#[test]
fn a_pause_for_a_stream_not_yet_adopted_is_latched_and_reports_nothing() {
    let (mut ctx, event_tx, event_rx) = adopted();
    ctx.state = AsioState::Playing;

    ctx.dispatch(
        AsioCommand::Pause {
            stream_id: STREAM + 1,
        },
        &event_tx,
    );

    assert_eq!(ctx.pending_transport, Some((STREAM + 1, false)));
    assert!(matches!(ctx.state, AsioState::Playing));
    assert!(
        event_rx.try_recv().is_err(),
        "a latched Pause announced a pause the control thread never made"
    );
}

/// A superseding StartStream during a deferred rebuild re-targets the adopted identity, and the
/// settle that follows reports the load that superseded, not the one it replaced.
#[test]
fn a_superseded_adoption_reports_the_load_that_replaced_it() {
    let (mut ctx, event_tx, event_rx) = adopted();

    ctx.adopt_stream_identity(
        &event_tx,
        STREAM + 1,
        OPENED_UNDER + 1,
        44_100,
        2,
        180.0,
        0.0,
        Arc::new(AtomicU64::new(0)),
    );
    while event_rx.try_recv().is_ok() {}
    ctx.settle_transport(false, &event_tx);

    assert!(matches!(
        event_rx.try_recv(),
        Ok(AsioEvent::StateChange {
            opened_under,
            state: PlaybackState::Active
        }) if opened_under == OPENED_UNDER + 1
    ));
}
