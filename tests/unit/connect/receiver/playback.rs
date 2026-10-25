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

/// An event of the load the controller handed the player at `engine_gen`, its first seq.
fn of_engine(engine_gen: u64) -> EventStamp {
    EventStamp { engine_gen, seq: 0 }
}

/// A controller already announcing `media`, with the channel to the session in the test's hand.
fn announcing(media: MediaInfo) -> (PlaybackController, mpsc::Receiver<PlaybackNotifyEvent>) {
    let (tx, rx) = mpsc::channel(8);
    let mut controller = PlaybackController::new(SpeakerBridge::new(), tx);
    controller.current_media = Some(media);
    (controller, rx)
}

/// A track the bridge cannot hand to the player produces no `PlayerEvent`, and `on_prepared` /
/// `on_playback_error` are the only ways out of `Preparing`, both of them fed by such an event.
/// A refusal that goes unanswered here leaves the controller announcing `Preparing` for the
/// rest of the session, and the phone never learns the track will not start.
#[tokio::test]
async fn media_the_bridge_cannot_load_does_not_strand_the_state() {
    let (tx, mut to_session) = mpsc::channel(8);
    let mut controller = PlaybackController::new(SpeakerBridge::new(), tx);

    controller.set_media(media("88264189", None), 1).await;

    assert_eq!(
        controller.state,
        PbState::Stopped,
        "a refused load must not leave the state stuck where only a PlayerEvent could move it"
    );
    assert!(
        to_session.try_recv().is_ok(),
        "the refusal has to reach the controller, not just the log"
    );
}

#[tokio::test]
async fn a_measurement_for_the_announced_track_corrects_its_length() {
    // The queue window can name a track without any length at all, and the decoder is then the
    // only source there is. Before this, its measurement reached the OS controls and stopped.
    let (mut controller, mut to_session) = announcing(media("88264189", None));

    controller
        .on_duration_measured(214_000, Some("88264189"), of_engine(0))
        .await;

    assert!(
        to_session.try_recv().is_ok(),
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

/// A refusal can outlive the track it judged, and so can a measurement. Two unidentified sides are
/// not the same track (what `same_track` exists to say), and an event of a retired engine follows
/// the staleness discipline every other bridge event does.
#[tokio::test]
async fn a_measurement_for_another_track_or_a_retired_engine_changes_nothing() {
    for (track_id, engine_gen) in [(Some("120002099"), 0), (None, 0), (Some("88264189"), 7)] {
        let (mut controller, mut to_session) = announcing(media("88264189", None));

        controller
            .on_duration_measured(214_000, track_id, of_engine(engine_gen))
            .await;

        assert!(
            to_session.try_recv().is_err(),
            "{track_id:?}, engine {engine_gen}: the measurement was announced"
        );
        assert_eq!(
            controller
                .current_media
                .and_then(|m| m.metadata)
                .and_then(|m| m.duration),
            None,
            "{track_id:?}, engine {engine_gen}: a length was charged to the announced track"
        );
    }
}

#[test]
fn a_load_owns_the_events_of_its_generation_from_its_seq_on() {
    assert!(
        !EventStamp {
            engine_gen: 1,
            seq: 9
        }
        .is_from(1, 10)
    );
    assert!(
        EventStamp {
            engine_gen: 1,
            seq: 10
        }
        .is_from(1, 10)
    );
    // A stop or a re-arm the player took over the same load since.
    assert!(
        EventStamp {
            engine_gen: 1,
            seq: 11
        }
        .is_from(1, 10)
    );
    assert!(
        !EventStamp {
            engine_gen: 0,
            seq: 11
        }
        .is_from(1, 10)
    );
}

fn preparing_the_casts_first_track() -> (PlaybackController, mpsc::Receiver<PlaybackNotifyEvent>) {
    let (mut controller, notices) = announcing(media("88264189", None));
    controller.state = PbState::Preparing;
    controller.engine_gen = 1;
    controller.load_seq = 10;
    (controller, notices)
}

/// The listener's own track can end while the cast's first track is fetched, its last events
/// forwarded after the cast took the player. Taken as the cast's, that end skipped the cast's
/// first track, or with nothing after it left the controller `Completed` under a track that
/// plays, which the phone could no longer pause.
#[tokio::test]
async fn a_local_tracks_end_forwarded_late_leaves_the_casts_first_track_alone() {
    let (mut controller, mut notices) = preparing_the_casts_first_track();

    controller
        .on_playback_completed(EventStamp {
            engine_gen: 1,
            seq: 9,
        })
        .await;

    assert_eq!(controller.state, PbState::Preparing);
    assert!(
        notices.try_recv().is_err(),
        "the end of a track the cast never played moved the cast"
    );
}

/// Same window, the local decoder failing: taken as the cast's, it settled the controller to
/// `Stopped`, and the cast's own `Ready` was then ignored.
#[tokio::test]
async fn a_local_tracks_failure_forwarded_late_leaves_the_casts_first_track_alone() {
    let (mut controller, mut notices) = preparing_the_casts_first_track();

    controller
        .on_playback_error(
            "decode failed",
            EventStamp {
                engine_gen: 1,
                seq: 9,
            },
        )
        .await;

    assert_eq!(controller.state, PbState::Preparing);
    assert!(notices.try_recv().is_err());
}

/// The skip that loads the cast's next track pauses the previous one, and an exclusive or ASIO
/// backend reports that pause after the load. Heard as the new track's own it set the intent to
/// `Paused` and told the phone so, and the `Pause` the phone sent in the window after it was
/// swallowed (`pause` does nothing once the intent already reads `Paused`) while the backend
/// started the track anyway.
#[tokio::test]
async fn the_pause_of_the_previous_load_is_not_the_new_loads() {
    let (mut controller, mut notices) = preparing_the_casts_first_track();
    controller.state = PbState::Started;
    controller.play_state = PbPlayState::Playing;

    controller
        .on_status_updated(
            PlayerState::Paused,
            EventStamp {
                engine_gen: 1,
                seq: 9,
            },
        )
        .await;

    assert_eq!(controller.play_state, PbPlayState::Playing);
    assert!(
        notices.try_recv().is_err(),
        "the previous load's pause was announced to the phone as the new track's"
    );

    controller.pause().await;

    assert_eq!(
        controller.play_state,
        PbPlayState::Paused,
        "the phone's Pause was swallowed"
    );
}

#[tokio::test]
async fn the_casts_own_end_still_moves_its_queue_on() {
    let (mut controller, mut notices) = preparing_the_casts_first_track();
    controller.state = PbState::Started;

    controller
        .on_playback_completed(EventStamp {
            engine_gen: 1,
            seq: 10,
        })
        .await;

    assert_eq!(controller.state, PbState::Completed);
    let mut asked_for_next = false;
    while let Ok(notice) = notices.try_recv() {
        asked_for_next |= matches!(
            notice,
            PlaybackNotifyEvent::Internal(PlaybackInternalEvent::MediaCompleted { .. })
        );
    }
    assert!(
        asked_for_next,
        "the cast's own end did not ask for the next track"
    );
}
