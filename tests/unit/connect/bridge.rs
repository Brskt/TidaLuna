//! Tests for the player-event translation in `src/connect/bridge.rs`, attached by `#[path]`.
//!
//! The statics this drives are process-wide. Both cases live in one test rather than racing
//! each other under the parallel harness.

use super::*;
use crate::player::{DeviceErrorKind, MediaErrorCode};

/// libtest runs the crate several threads wide and `BRIDGE_TX`/`BRIDGE_HAS_CLIENT` are
/// process-wide: a second test replacing the channel mid-case would make the first one
/// forward into the wrong receiver and fail for a reason that is not its own. Taken first by
/// every case here and by the receiver's routing tests, which run the real loop over these
/// statics, the way `tests/unit/audio/preload.rs` does for `PRELOAD_STATE`.
pub(crate) static BRIDGE_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) fn has_client() -> bool {
    BRIDGE_HAS_CLIENT.load(Ordering::Acquire)
}

/// Adopted, a promotion made the Next that followed step nothing. A natural end still reaches
/// the receiver.
#[tokio::test]
async fn a_completion_reaches_the_receiver_and_a_promotion_does_not() {
    let _serialised = BRIDGE_TESTS.lock().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    set_active(Some(tx));
    set_client_connected(true);

    forward(
        &PlayerEvent::CrossfadePromoted(1, Some("120002099".to_string())),
        LoadOrigin::Connect,
    );
    assert!(
        rx.try_recv().is_err(),
        "a local promotion reached the controller's queue"
    );

    forward(
        &PlayerEvent::StateChange(PlaybackState::Completed, 1),
        LoadOrigin::Connect,
    );
    let completed = rx.try_recv().expect("a natural end reaches the receiver");
    assert!(
        matches!(completed, BridgeEvent::PlaybackCompleted { .. }),
        "a real completion still has to ask the controller for the next track"
    );

    set_client_connected(false);
    set_active(None);
}

/// A dead network has to reach the controller, or the phone renders a track that never advances.
///
/// `NetworkLost` carried no arm of its own and fell into the translation's trailing wildcard;
/// nothing reached the receiver. The `Stopped` that follows arrives as `Idle`, which
/// `on_status_updated` ignores while `PbState::Started` still stands, and the status recomputed
/// from it re-asserts `Playing`. The phone was told the opposite of what happened. Routed as an
/// error instead, because that is the one arm the receiver answers by settling to `Stopped` first.
#[tokio::test]
async fn a_dead_network_reaches_the_controller() {
    let _serialised = BRIDGE_TESTS.lock().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    set_active(Some(tx));
    set_client_connected(true);

    forward(&PlayerEvent::NetworkLost(1), LoadOrigin::Connect);

    let lost = rx.try_recv().expect("a dead network reaches the receiver");
    assert!(
        matches!(lost, BridgeEvent::PlaybackError { .. }),
        "swallowed, the controller goes on showing a track that will never advance"
    );

    set_client_connected(false);
    set_active(None);
}

#[tokio::test]
async fn no_client_means_no_forwarding() {
    let _serialised = BRIDGE_TESTS.lock().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    set_active(Some(tx));
    set_client_connected(false);

    forward(
        &PlayerEvent::StateChange(PlaybackState::Completed, 1),
        LoadOrigin::Connect,
    );

    assert!(
        rx.try_recv().is_err(),
        "forwarded with no controller attached"
    );
    set_active(None);
}

fn stamped_seq(event: &BridgeEvent) -> Option<u32> {
    match event {
        BridgeEvent::Prepared { from }
        | BridgeEvent::StatusUpdated { from, .. }
        | BridgeEvent::ProgressUpdated { from, .. }
        | BridgeEvent::PlaybackCompleted { from, .. }
        | BridgeEvent::PlaybackError { from, .. }
        | BridgeEvent::DurationMeasured { from, .. } => Some(from.seq),
        BridgeEvent::LocalLoad(_) => None,
    }
}

/// A failure needs the seq its load stamped as much as an end does (see `EventStamp`).
#[tokio::test]
async fn ready_progress_duration_end_and_failures_reach_the_receiver_with_their_seq() {
    let _serialised = BRIDGE_TESTS.lock().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    set_active(Some(tx));
    set_client_connected(true);

    for event in [
        PlayerEvent::StateChange(PlaybackState::Ready, 7),
        PlayerEvent::StateChange(PlaybackState::Completed, 7),
        PlayerEvent::TimeUpdate(12.0, 7),
        PlayerEvent::Duration(180.0, 7, Some("120002099".to_string())),
        PlayerEvent::MediaError {
            error: "decode failed".to_string(),
            code: MediaErrorCode::UnreadableFile,
            seq: 7,
            product_id: None,
        },
        PlayerEvent::NetworkLost(7),
    ] {
        forward(&event, LoadOrigin::Connect);
        let forwarded = rx.try_recv().expect("the receiver's own load reaches it");
        assert_eq!(
            stamped_seq(&forwarded),
            Some(7),
            "forwarded without the seq of its load: {event:?}"
        );
    }

    set_client_connected(false);
    set_active(None);
}

/// Every kind of event the player emits, every state included.
fn events_of_a_track() -> Vec<PlayerEvent> {
    let mut events: Vec<PlayerEvent> = [
        PlaybackState::Ready,
        PlaybackState::Active,
        PlaybackState::Paused,
        PlaybackState::Stopped,
        PlaybackState::Seeking,
        PlaybackState::Idle,
        PlaybackState::Completed,
    ]
    .into_iter()
    .map(|state| PlayerEvent::StateChange(state, 1))
    .collect();
    events.extend([
        PlayerEvent::TimeUpdate(12.0, 1),
        PlayerEvent::Duration(180.0, 1, Some("120002099".to_string())),
        PlayerEvent::CrossfadePromoted(1, Some("120002099".to_string())),
        PlayerEvent::AudioDevices(Vec::new(), None),
        PlayerEvent::DeviceError(DeviceErrorKind::Unknown),
        PlayerEvent::MediaFormat {
            codec: "flac",
            sample_rate: 44_100,
            output_sample_rate: 44_100,
            bit_depth: Some(16),
            channels: 2,
            bytes: 1,
        },
        PlayerEvent::Version("0"),
        PlayerEvent::MediaError {
            error: "load failed".to_string(),
            code: MediaErrorCode::NoSuchFile,
            seq: 1,
            product_id: None,
        },
        PlayerEvent::NetworkLost(1),
        PlayerEvent::ReplayRequest {
            track: crate::state::TrackInfo {
                url: "https://example.invalid/a.flac".to_string(),
                key: String::new(),
                format: "flac".to_string(),
                product_id: None,
            },
            expected_gen: 1,
            position: None,
            play: true,
        },
        PlayerEvent::VolumeSync(50.0),
    ]);
    events
}

/// A phone attached to this speaker, casting or not, hears nothing of this host's own music.
/// The receiver still hears the same events of its own load.
#[tokio::test]
async fn a_local_load_is_never_the_receivers_to_hear() {
    let _serialised = BRIDGE_TESTS.lock().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    set_active(Some(tx));
    set_client_connected(true);

    for event in events_of_a_track() {
        forward(&event, LoadOrigin::Local);
        assert!(
            rx.try_recv().is_err(),
            "an event of a local load reached the receiver: {event:?}"
        );
    }

    let mut heard = 0;
    for event in events_of_a_track() {
        forward(&event, LoadOrigin::Connect);
        while rx.try_recv().is_ok() {
            heard += 1;
        }
    }
    assert_eq!(
        heard, 10,
        "the receiver's own load stopped reaching it: every state but seeking, the tick, the \
         length and the two failures"
    );

    set_client_connected(false);
    set_active(None);
}
