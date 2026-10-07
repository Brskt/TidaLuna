//! Tests for the player-event translation in `src/connect/bridge.rs`, attached by `#[path]`.
//!
//! The statics this drives are process-wide. Both cases live in one test rather than racing
//! each other under the parallel harness.

use super::*;

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

    forward(&PlayerEvent::CrossfadePromoted(
        1,
        Some("120002099".to_string()),
    ));
    assert!(
        rx.try_recv().is_err(),
        "a local promotion reached the controller's queue"
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

#[tokio::test]
async fn no_client_means_no_forwarding() {
    let _serialised = BRIDGE_TESTS.lock().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    set_active(Some(tx));
    set_client_connected(false);

    forward(&PlayerEvent::StateChange(PlaybackState::Completed, 1));

    assert!(
        rx.try_recv().is_err(),
        "forwarded with no controller attached"
    );
    set_active(None);
}
