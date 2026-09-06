//! Tests for the player-event translation in `src/connect/bridge.rs`, attached by `#[path]`.
//!
//! The statics this drives are process-wide. Both cases live in one test rather than racing
//! each other under the parallel harness.

use super::*;

/// libtest runs the crate several threads wide and `BRIDGE_TX`/`BRIDGE_HAS_CLIENT` are
/// process-wide: a second test replacing the channel mid-case would make the first one
/// forward into the wrong receiver and fail for a reason that is not its own. Taken first by
/// every case here, the way `tests/unit/audio/preload.rs` does for `PRELOAD_STATE`.
static BRIDGE_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A crossfade promotion and a natural end look alike and mean opposite things.
///
/// A completion says the player stopped and the controller owes it the next track. A promotion
/// says the next track is already playing, seconds in. Translated as a completion, the
/// controller ran its advance-and-prepare path over a track it did not need to prepare, and the
/// listener heard it stop and restart from zero; the same crossfade ran unbroken with no phone
/// attached.
#[tokio::test]
async fn a_promotion_is_not_translated_as_a_completion() {
    let _serialised = BRIDGE_TESTS.lock().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    set_active(Some(tx));
    set_client_connected(true);

    forward(&PlayerEvent::CrossfadePromoted(1, None));
    let promoted = rx.try_recv().expect("a promotion reaches the receiver");
    assert!(
        matches!(promoted, BridgeEvent::CrossfadeTransitioned { .. }),
        "a promotion routed as a completion makes the controller reload the playing track"
    );

    forward(&PlayerEvent::StateChange(PlaybackState::Completed, 1));
    let completed = rx.try_recv().expect("a natural end reaches the receiver");
    assert!(
        matches!(completed, BridgeEvent::PlaybackCompleted { .. }),
        "a real completion still has to ask the controller for the next track"
    );

    set_client_connected(false);
    set_active(None);
}

/// A promotion that cannot name its track leaves the receiver guessing which one moved.
///
/// The player knows exactly what it promoted: it stamps the very next event, the measured
/// length, with that same id. Dropped in translation, the receiver falls back to arithmetic on
/// its own queue index, and the length measured in the same breath has nothing to be judged
/// against: it names the incoming track while the receiver still names the outgoing one.
#[tokio::test]
async fn a_promotion_names_the_track_it_promoted() {
    let _serialised = BRIDGE_TESTS.lock().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    set_active(Some(tx));
    set_client_connected(true);

    forward(&PlayerEvent::CrossfadePromoted(
        1,
        Some("120002099".to_string()),
    ));

    let promoted = rx.try_recv().expect("a promotion reaches the receiver");
    assert!(
        matches!(
            promoted,
            BridgeEvent::CrossfadeTransitioned { ref track_id, .. }
                if track_id.as_deref() == Some("120002099")
        ),
        "the promoted track's id was dropped in translation"
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

    forward(&PlayerEvent::NetworkLost);

    let lost = rx.try_recv().expect("a dead network reaches the receiver");
    assert!(
        matches!(lost, BridgeEvent::PlaybackError { .. }),
        "swallowed, the controller goes on showing a track that will never advance"
    );

    set_client_connected(false);
    set_active(None);
}

/// Nobody connected means nobody to notify: the whole chain stays dormant, and a promotion is
/// not an exception to it.
#[tokio::test]
async fn no_client_means_no_forwarding() {
    let _serialised = BRIDGE_TESTS.lock().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    set_active(Some(tx));
    set_client_connected(false);

    forward(&PlayerEvent::CrossfadePromoted(1, None));

    assert!(
        rx.try_recv().is_err(),
        "forwarded with no controller attached"
    );
    set_active(None);
}
