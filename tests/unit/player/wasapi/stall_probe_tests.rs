//! Tests for `src/player/wasapi.rs`, attached to it by `#[path]`.
//!
//! The stall probe is what turned "a seek takes 1.2-2.5s" into "`IAudioClient::Reset()`
//! takes 1.2-2.5s". Its bar is a formula rather than a chosen number, and its parked
//! state is what keeps a quiet minute from reading as a fault.

use super::*;

fn probe_for(buffer_frames: u32, sample_rate: u32) -> StallProbe {
    let mut probe = StallProbe::new();
    probe.retune(buffer_frames, sample_rate);
    probe
}

#[test]
fn the_bar_follows_the_negotiated_period() {
    // One event wait plus two device periods. 882 frames at 44.1kHz is the 20ms period
    // the exclusive path asks for, and what the Focusrite endpoint opens at.
    assert_eq!(probe_for(882, 44100).threshold, Duration::from_millis(90));
    assert_eq!(probe_for(1920, 96000).threshold, Duration::from_millis(90));
    // A driver that realigns the buffer to 10ms moves the bar with it.
    assert_eq!(probe_for(441, 44100).threshold, Duration::from_millis(70));
}

#[test]
fn the_bar_always_clears_one_event_wait() {
    // A wait that simply times out is not a stall. If the bar ever fell to or below the
    // wait ceiling, every idle period would be reported.
    for frames in [64u32, 441, 882, 1024, 4096] {
        let probe = probe_for(frames, 44100);
        assert!(
            probe.threshold > Duration::from_millis(u64::from(EVENT_WAIT_RUNNING_MS)),
            "{frames} frames left the bar at {:?}",
            probe.threshold
        );
    }
}

#[test]
fn no_open_stream_neither_panics_nor_leaves_a_zero_bar() {
    // Before the first stream the loop is parked in Idle: the only span this bar judges
    // is the client open itself, which is worth seeing whatever it costs.
    let probe = probe_for(0, 0);
    assert_eq!(
        probe.threshold,
        Duration::from_millis(u64::from(EVENT_WAIT_RUNNING_MS))
    );
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

#[test]
fn lap_reopens_the_span_even_when_it_was_parked() {
    // The lap at the top of the render loop is the catch-all every `continue` lands on;
    // it has to leave a span open whatever state it found.
    let mut probe = probe_for(882, 44100);
    probe.park();

    probe.lap("render pass");

    assert!(probe.since.is_some());
}
