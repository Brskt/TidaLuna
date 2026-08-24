//! Tests for `src/connect/receiver/playback.rs`, attached to it by `#[path]`.
//!
//! Only the measured-length correction is exercised. `NotifyMediaChanged` is the sole message
//! carrying a length, so correcting one means sending that message a second time, and when it
//! is sent at all is the whole rule.

use super::*;

fn media(id: &str, duration: Option<u64>) -> MediaInfo {
    serde_json::from_value(serde_json::json!({
        "itemId": "i-1",
        "mediaId": id,
        "metadata": { "duration": duration },
    }))
    .expect("ids and metadata are all this needs")
}

/// A controller already announcing `media`, with the broadcast channel in the test's hand.
fn announcing(media: MediaInfo) -> (PlaybackController, mpsc::Receiver<PlaybackNotifyEvent>) {
    let (tx, rx) = mpsc::channel(8);
    let mut controller = PlaybackController::new(SpeakerBridge::new(), tx);
    controller.current_media = Some(media);
    (controller, rx)
}

/// A track the bridge cannot hand to the player produces no `PlayerEvent`, and `on_prepared` /
/// `on_playback_error` are the only ways out of `Preparing`, both of them fed by such an event.
/// So a refusal that goes unanswered here leaves the controller announcing `Preparing` for the
/// rest of the session, and the phone never learns the track will not start.
#[tokio::test]
async fn media_the_bridge_cannot_load_does_not_strand_the_state() {
    let (tx, mut broadcasts) = mpsc::channel(8);
    let mut controller = PlaybackController::new(SpeakerBridge::new(), tx);

    controller.set_media(media("88264189", None), 1).await;

    assert_eq!(
        controller.state,
        PbState::Stopped,
        "a refused load must not leave the state stuck where only a PlayerEvent could move it"
    );
    assert!(
        broadcasts.try_recv().is_ok(),
        "the refusal has to reach the controller, not just the log"
    );
}

#[tokio::test]
async fn a_measurement_for_the_announced_track_corrects_its_length() {
    // The queue window can name a track without any length at all, and the decoder is then the
    // only source there is. Before this, its measurement reached the OS controls and stopped.
    let (mut controller, mut broadcasts) = announcing(media("88264189", None));

    controller
        .on_duration_measured(214_000, Some("88264189"), 0)
        .await;

    assert!(
        broadcasts.try_recv().is_ok(),
        "the corrected length was never announced to the controller"
    );
    assert_eq!(
        controller
            .current_media
            .and_then(|m| m.metadata)
            .and_then(|m| m.duration),
        Some(214_000)
    );
}

#[tokio::test]
async fn a_measurement_confirming_the_announced_length_announces_nothing() {
    // Re-sending `NotifyMediaChanged` is how a length is corrected, so a repeat the controller
    // does not need is a repeat it might render. Silence is the common path: the queue usually
    // carries the right figure already.
    let (mut controller, mut broadcasts) = announcing(media("88264189", Some(214_000)));

    controller
        .on_duration_measured(214_000, Some("88264189"), 0)
        .await;

    assert!(
        broadcasts.try_recv().is_err(),
        "an unchanged length was re-announced"
    );
}

#[tokio::test]
async fn a_measurement_naming_another_track_changes_nothing() {
    // A refusal can outlive the track it judged; a measurement can too.
    let (mut controller, mut broadcasts) = announcing(media("88264189", None));

    controller
        .on_duration_measured(214_000, Some("120002099"), 0)
        .await;

    assert!(broadcasts.try_recv().is_err());
    assert_eq!(
        controller
            .current_media
            .and_then(|m| m.metadata)
            .and_then(|m| m.duration),
        None,
        "a length measured on another track was charged to this one"
    );
}

#[tokio::test]
async fn a_measurement_naming_no_track_changes_nothing() {
    // Two unidentified sides are not the same track, which is what `same_track` exists to say.
    let (mut controller, mut broadcasts) = announcing(media("88264189", None));

    controller.on_duration_measured(214_000, None, 0).await;

    assert!(broadcasts.try_recv().is_err());
}

#[tokio::test]
async fn a_measurement_from_a_retired_engine_changes_nothing() {
    // Same staleness discipline every other bridge event follows.
    let (mut controller, mut broadcasts) = announcing(media("88264189", None));

    controller
        .on_duration_measured(214_000, Some("88264189"), 7)
        .await;

    assert!(broadcasts.try_recv().is_err());
}
