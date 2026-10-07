//! Tests for `src/player/thread/device.rs`, attached to it by `#[path]`. The switches themselves
//! are driven through the harness in `commands.rs`'s tests; this file holds the rule they share.

use super::SwitchRearm;
use crate::player::buffer::RamBuffer;
use crate::state::{RetainedTrack, TrackInfo};

fn retained(load_gen: u32) -> RetainedTrack {
    RetainedTrack {
        track: TrackInfo {
            url: "https://example.invalid/track-1.flac".to_string(),
            key: String::new(),
            format: "flac".to_string(),
            product_id: None,
        },
        load_gen,
    }
}

/// The DASH case. Asking the retained source first is what refused every exclusive or ASIO
/// switch on such a track.
#[test]
fn a_whole_track_is_reused_with_nothing_retained() {
    let whole = RamBuffer::from_complete(vec![1, 2, 3]);

    assert!(matches!(
        SwitchRearm::choose(Some(&whole), None),
        SwitchRearm::Reuse(buffer) if buffer.is_same_stream(&whole)
    ));
}

#[test]
fn a_whole_track_is_reused_even_with_a_source_retained() {
    let whole = RamBuffer::from_complete(vec![1, 2, 3]);

    assert!(matches!(
        SwitchRearm::choose(Some(&whole), Some(retained(7))),
        SwitchRearm::Reuse(buffer) if buffer.is_same_stream(&whole)
    ));
}

#[test]
fn a_streaming_track_replays_its_retained_source() {
    let (streaming, _writer) = RamBuffer::new_for_test(1_000);

    assert!(matches!(
        SwitchRearm::choose(Some(&streaming), Some(retained(7))),
        SwitchRearm::Replay(RetainedTrack { load_gen: 7, .. })
    ));
}

#[test]
fn a_streaming_track_with_nothing_retained_waits_for_the_load() {
    let (streaming, _writer) = RamBuffer::new_for_test(1_000);

    assert!(matches!(
        SwitchRearm::choose(Some(&streaming), None),
        SwitchRearm::AwaitLoad
    ));
}

#[test]
fn a_cancelled_track_is_never_reused() {
    let whole = RamBuffer::from_complete(vec![1, 2, 3]);
    whole.cancel();

    assert!(matches!(
        SwitchRearm::choose(Some(&whole), None),
        SwitchRearm::AwaitLoad
    ));
    assert!(matches!(
        SwitchRearm::choose(Some(&whole), Some(retained(7))),
        SwitchRearm::Replay(_)
    ));
}

#[test]
fn no_buffer_at_all_falls_to_what_is_retained() {
    assert!(matches!(
        SwitchRearm::choose(None, Some(retained(7))),
        SwitchRearm::Replay(_)
    ));
    assert!(matches!(
        SwitchRearm::choose(None, None),
        SwitchRearm::AwaitLoad
    ));
}
