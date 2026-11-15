//! Tests for `src/player/thread/device.rs`, attached to it by `#[path]`. The switches themselves
//! are driven through the `harness` fixture in `bypass.rs`'s tests; this file holds the rule
//! they share.

use super::{Held, HeldFacts, SwitchRearm};
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

/// A buffer outliving its track brings nothing back on a switch: an ended track restarted there
/// played what the listener had finished, and a released one superseded by a newer load played
/// the track they left. A live one always follows, whatever loads meanwhile.
#[test]
fn a_switch_brings_back_only_the_track_the_session_still_holds() {
    let whole = RamBuffer::from_complete(vec![1, 2, 3]);
    let facts = |has_track, has_identity, load_in_flight, published_by_buffer| HeldFacts {
        buffer: Some(&whole),
        has_track,
        has_identity,
        load_in_flight,
        published_by_buffer,
    };
    let track = |held: Held<'_>| matches!(held, Held::Track(b) if b.is_same_stream(&whole));
    // Live: held, even under a newer load that has not landed.
    assert!(track(Held::of(facts(true, true, true, false))));
    // Released, and still the session's source.
    assert!(track(Held::of(facts(false, true, false, true))));
    // Released, then a newer load published (and failed, or never landed).
    assert!(matches!(
        Held::of(facts(false, true, false, false)),
        Held::Gone
    ));
    // Released, a newer load in flight.
    assert!(matches!(
        Held::of(facts(false, true, true, true)),
        Held::Gone
    ));
    // Ended: no identity left.
    assert!(matches!(
        Held::of(facts(false, false, false, true)),
        Held::Gone
    ));
    let cold = HeldFacts {
        buffer: None,
        has_track: false,
        has_identity: false,
        load_in_flight: false,
        published_by_buffer: false,
    };
    assert!(matches!(Held::of(cold), Held::Unheld));
    // Gone brings nothing back, even with a source retained.
    assert!(matches!(
        SwitchRearm::for_held(Held::Gone, Some(retained(7))),
        SwitchRearm::AwaitLoad
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
