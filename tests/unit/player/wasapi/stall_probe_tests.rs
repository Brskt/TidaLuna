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
