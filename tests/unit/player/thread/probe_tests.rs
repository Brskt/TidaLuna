//! Tests for `probe_audio_format` in `src/player/thread/output.rs`, attached to it by `#[path]`.
//!
//! What the probe reports as a track's length is what the page is told, and the SDK's load of a
//! track waits on that report.

use super::probe_audio_format;
use crate::player::buffer::RamBuffer;
use crate::player::length::dash_fixture::{Durations, SEGMENTS, dash_track, seconds};

#[test]
fn a_dash_track_reports_the_length_its_fragments_add_up_to() {
    // The container's header declares none, and 0 is how the SDK reads "unknown".
    for how in [Durations::PerSample, Durations::Default] {
        let probe = probe_audio_format(&RamBuffer::from_complete(dash_track(&SEGMENTS, how)))
            .expect("the fixture probes");
        assert!(
            (probe.duration - seconds(&SEGMENTS)).abs() < 1e-6,
            "{} s for {} s of frames",
            probe.duration,
            seconds(&SEGMENTS)
        );
        assert_eq!(probe.sample_rate, 44_100);
        assert_eq!(probe.channels, 2);
    }
}
