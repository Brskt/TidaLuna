//! Tests for `src/ui/flush.rs`, attached to it by `#[path]`. Only the measured-length
//! decision is exercised here: it was split out of the `Duration` arm precisely so the rule
//! could be stated without the shared slot, the app state or a CEF task runner.

use super::{MediaControlAction, settle_measured_duration};
use crate::state::TrackMetadata;

/// What the frontend announced. The title carries the id so a length published beside the
/// wrong name is visible in the assertion rather than merely counted.
fn announced(id: &str) -> TrackMetadata {
    TrackMetadata {
        title: format!("title {id}"),
        artist: format!("artist {id}"),
        quality: String::new(),
        id: Some(id.to_string()),
    }
}

/// A length is recorded under the track it was measured on, never the one announced since: the
/// tag was read from the shared slot at delivery time. A's length then became B's once B announced
/// itself. Publishing 71.8s beside B's name is the hardware-reproduced symptom.
#[test]
fn a_length_keeps_its_own_track_and_is_not_published_beside_the_announced_one() {
    let (measured, action) =
        settle_measured_duration(71.8, Some("A".to_string()), Some(&announced("B")));

    assert_eq!(
        measured.track_id.as_deref(),
        Some("A"),
        "the measurement was recorded under the announced track instead of its own"
    );
    assert_eq!(measured.secs, 71.8);
    assert!(
        matches!(action, MediaControlAction::None),
        "a length went out beside a track it was not measured on"
    );
}

#[test]
fn a_length_is_published_beside_the_track_it_was_measured_on() {
    let (_, action) = settle_measured_duration(319.0, Some("A".to_string()), Some(&announced("A")));

    match action {
        MediaControlAction::SetMetadata {
            title,
            artist,
            duration,
        } => {
            assert_eq!(title, "title A");
            assert_eq!(artist, "artist A");
            assert_eq!(duration, Some(319.0));
        }
        _ => panic!("the length was withheld from its own track"),
    }
}

#[test]
fn a_measurement_naming_no_track_is_published_under_none() {
    // A re-arm can still measure without an id. Fail closed: nothing is announced, and the
    // slot turns the measurement away too; an untagged length is lost, not held. That is
    // why the gapless advance now carries the id the preload was tagged with instead of
    // relying on this arm, which left every advanced track with no duration at all.
    let (measured, action) = settle_measured_duration(200.0, None, Some(&announced("A")));

    assert_eq!(measured.track_id, None);
    assert!(
        matches!(action, MediaControlAction::None),
        "an unidentified length was lent to whichever track was on screen"
    );
}

#[test]
fn nothing_announced_still_records_the_measurement() {
    // First track after startup: the slot is empty until the frontend's first frame. The
    // measurement must survive it, or the length is lost before anyone can claim it.
    let (measured, action) = settle_measured_duration(150.0, Some("A".to_string()), None);

    assert_eq!(measured.track_id.as_deref(), Some("A"));
    assert_eq!(measured.secs, 150.0);
    assert!(matches!(action, MediaControlAction::None));
}
