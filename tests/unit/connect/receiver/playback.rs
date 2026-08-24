//! Tests for `src/connect/receiver/playback.rs`, attached to it by `#[path]`.
//!
//! `Preparing` is the state only a `PlayerEvent` can leave, which makes a load the bridge
//! refused the one case the controller has to answer for on its own.

use super::*;

fn media(id: &str, duration: Option<u64>) -> MediaInfo {
    serde_json::from_value(serde_json::json!({
        "itemId": "i-1",
        "mediaId": id,
        "metadata": { "duration": duration },
    }))
    .expect("ids and metadata are all this needs")
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
