//! Tests for `src/player/thread/device.rs`, attached to it by `#[path]`. The switches themselves
//! are driven through the `harness` fixture in `bypass.rs`'s tests; this file holds the rule
//! they share.

use super::{Held, HeldFacts, ReopenFacts, SwitchRearm};
use crate::player::PlayerEvent;
use crate::player::buffer::RamBuffer;
use crate::player::thread::harness::harness;
use crate::state::{RetainedTrack, TrackInfo};

/// The reopen checks a default, then opens it: one gone in between waits armed for the next
/// device event. The page asked for nothing and hears nothing.
#[test]
fn a_failed_automatic_reopen_stays_armed_and_silent() {
    let mut h = harness();
    h.player.is_playing = false;
    h.player.current_buffer = Some(RamBuffer::from_complete(vec![0; 16]));
    // Present when checked, gone by the open: an absent named device resolves to nothing.
    h.player.current_device_id = Some("Speakers (Gone DAC)".to_string());
    h.player.output_is_default = true;
    h.player.default_output_present = || true;
    h.player.reopen_default_on_arrival = true;

    h.player.reopen_lost_default_output();

    assert!(
        h.player.reopen_default_on_arrival,
        "the failed try spent the next device event's retry"
    );
    let events = h.events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, PlayerEvent::DeviceError(_))),
        "the page heard an error it never asked about: {events:?}"
    );
}

/// The official engine re-inits on a default that appears after the last one went away. Ours
/// reopens only that output: never beside an exclusive or ASIO backend, under a
/// load, for a dead page, or while a Play waits on the SDK's answer to its NPD05.
#[test]
fn a_lost_default_output_reopens_only_when_nothing_else_owns_the_moment() {
    let ready = ReopenFacts {
        armed: true,
        stream_open: false,
        output_is_default: true,
        has_track: true,
        is_playing: false,
        load_in_flight: false,
        orphaned: false,
        bypass_busy: false,
    };
    assert!(ready.allow(), "the default came back and nothing reopened");
    type Block = fn(&mut ReopenFacts);
    let blockers: [(Block, &str); 8] = [
        (|f| f.armed = false, "no loss armed it"),
        (|f| f.stream_open = true, "a stream is open"),
        (|f| f.output_is_default = false, "a named output"),
        (|f| f.has_track = false, "no track held"),
        (|f| f.is_playing = true, "a Play awaits the SDK"),
        (|f| f.load_in_flight = true, "a load opens its own"),
        (|f| f.orphaned = true, "the page is gone"),
        (|f| f.bypass_busy = true, "exclusive or ASIO holds it"),
    ];
    for (block, what) in blockers {
        let mut facts = ready;
        block(&mut facts);
        assert!(!facts.allow(), "reopened although {what}");
    }
}

fn retained(load_gen: u32) -> RetainedTrack {
    RetainedTrack {
        track: TrackInfo {
            url: "https://example.invalid/track-1.flac".to_string(),
            key: String::new(),
            format: "flac".to_string(),
            product_id: None,
            gains: crate::player::normalization::TrackGains::UNITY,
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
