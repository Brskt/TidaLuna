//! Tests for `src/player/wasapi/render.rs`, attached to it by `#[path]`.

use super::*;
use crate::player::PlaybackState;

#[test]
fn push_reclaims_played_prefix() {
    let mut ctx = RenderContext::new();
    ctx.current_stream_id = Some(7);

    ctx.handle_push_pcm(7, vec![1u8; 1000]);
    assert_eq!(ctx.pcm_data.len(), 1000);

    // Simulate the render loop consuming 600 bytes.
    ctx.write_cursor = 600;
    ctx.handle_push_pcm(7, vec![2u8; 500]);
    // Played prefix reclaimed: only the 400 unplayed + 500 new bytes remain.
    assert_eq!(ctx.write_cursor, 0);
    assert_eq!(ctx.pcm_data.len(), 900);
    assert_eq!(ctx.pcm_data[0], 1);
    assert_eq!(ctx.pcm_data[400], 2);

    // A stale stream's push is dropped.
    ctx.handle_push_pcm(8, vec![3u8; 100]);
    assert_eq!(ctx.pcm_data.len(), 900);
}

#[test]
fn reset_for_seek_credits_discarded_audio_as_consumed() {
    let mut ctx = RenderContext::new();
    ctx.current_stream_id = Some(7);
    ctx.pcm_sample_rate = 44100;

    ctx.handle_push_pcm(7, vec![1u8; 1000]);
    ctx.write_cursor = 600;
    // The reset answers the seek on `event_tx`; the receiver stays alive, letting the
    // send behave as it does in production, where the player thread is listening.
    let (event_tx, _event_rx) = mpsc::channel();
    ctx.handle_reset_for_seek(&event_tx, 7, 1, 0.0);

    // The 400 unplayed bytes were discarded: credited to the throttle.
    assert_eq!(ctx.consumed.load(Relaxed), 400);
    assert_eq!(ctx.pcm_data.len(), 0);
    assert_eq!(ctx.write_cursor, 0);
}

// `Reset()` reaches the driver and has been measured between 1.1s and 3.4s on a
// Focusrite endpoint; the render thread must only pay it when the endpoint really holds
// frames. These pin that rule down as a truth table rather than as a list of the paths
// that happen to lead here today.

fn ctx_with(client_started: bool, endpoint_dirty: bool) -> RenderContext {
    let mut ctx = RenderContext::new();
    ctx.client_started = client_started;
    ctx.endpoint_dirty = endpoint_dirty;
    ctx
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

// The stall probe is what turned "a seek takes 1.2-2.5s" into "`IAudioClient::Reset()`
// takes 1.2-2.5s". Its bar is a formula rather than a chosen number, and its parked
// state is what keeps a quiet minute from reading as a fault.

fn probe_for(buffer_frames: u32, sample_rate: u32) -> StallProbe {
    let mut probe = StallProbe::new();
    probe.retune(buffer_frames, sample_rate);
    probe
}

#[test]
fn a_parked_probe_never_reports() {
    let mut probe = probe_for(882, 44100);
    probe.arm();
    probe.park();

    // The Idle and Paused arms block on `recv` with no deadline; measuring that would
    // call every quiet minute a stall.
    assert_eq!(
        probe.overrun(Instant::now() + Duration::from_secs(600)),
        None
    );
}

#[test]
fn an_armed_span_is_reported_only_past_the_bar() {
    let mut probe = probe_for(882, 44100);
    probe.arm();
    let armed_at = probe.since.expect("arm opens a span");

    assert_eq!(probe.overrun(armed_at + Duration::from_millis(20)), None);
    assert_eq!(probe.overrun(armed_at + Duration::from_millis(89)), None);
    assert_eq!(
        probe.overrun(armed_at + Duration::from_millis(1282)),
        Some(Duration::from_millis(1282))
    );
}

// What a `Play` means to the render loop, stated once for each state it can reach. The drain
// serving `Playing` handles a whole burst of commands before it re-reads its own state, so
// "we are playing" is a premise no handler may carry from one command to the next.

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
fn an_adopted_stream_announces_its_length_then_its_start_state_under_its_load() {
    for (start_paused, reported) in [
        (false, PlaybackState::Active),
        (true, PlaybackState::Paused),
    ] {
        let (ctx, event_rx) = adopting(start_paused);

        assert_eq!(
            matches!(ctx.state, RenderState::Paused),
            start_paused,
            "start_paused {start_paused}"
        );
        assert_eq!(
            matches!(ctx.state, RenderState::Playing),
            !start_paused,
            "start_paused {start_paused}"
        );
        assert_eq!(ctx.opened_under, OPENED_UNDER);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(ExclusiveEvent::Duration { stream_id: 7, .. })
        ));
        assert!(
            matches!(
                event_rx.try_recv(),
                Ok(ExclusiveEvent::StateChange {
                    opened_under: OPENED_UNDER,
                    state
                }) if state == reported
            ),
            "start_paused {start_paused}"
        );
    }
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
