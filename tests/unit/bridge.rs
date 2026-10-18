//! Tests for `src/bridge.rs`, attached to it by `#[path]`.
//!
//! The renderer's queue reads a state event's `cast` field before acting on a completion. Its
//! self-load flag names the last load that page issued, which a Connect load never touches: a
//! stale flag advanced the local queue over the cast at every track boundary.

use super::PlayerBridgeEvent;
use crate::player::LoadOrigin;

#[test]
fn a_state_handed_over_during_a_cast_says_so() {
    let event = PlayerBridgeEvent::state("completed", 7, LoadOrigin::Connect);
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        serde_json::json!({ "t": "state", "seq": 7, "v": "completed", "cast": true })
    );
}

/// The counterweight, and the shape every renderer already parses: this host's own states carry
/// no `cast` field.
#[test]
fn a_state_of_this_hosts_own_load_is_unchanged() {
    let event = PlayerBridgeEvent::state("completed", 7, LoadOrigin::Local);
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        serde_json::json!({ "t": "state", "seq": 7, "v": "completed" })
    );
}

/// A refused load mints no generation, so its answer carries no seq: one would outrank the
/// cast's own events in the renderer, which drops every event older than the newest it saw.
#[test]
fn the_refusal_answer_carries_no_seq() {
    assert_eq!(
        serde_json::to_value(PlayerBridgeEvent::cast_owns_player(None)).unwrap(),
        serde_json::json!({ "t": "mediacastowned", "v": null })
    );
}

#[test]
fn a_refusal_names_the_load_it_answers() {
    assert_eq!(
        serde_json::to_value(PlayerBridgeEvent::cast_owns_player(Some(7))).unwrap(),
        serde_json::json!({ "t": "mediacastowned", "v": { "load": 7 } })
    );
}

/// A load the controller's queue plays is answered by name too, and with no seq: the receiver's
/// load of the track mints the generations that follow.
#[test]
fn a_routed_load_names_the_load_it_answers() {
    assert_eq!(
        serde_json::to_value(PlayerBridgeEvent::cast_routed(7)).unwrap(),
        serde_json::json!({ "t": "mediacastrouted", "v": { "load": 7 } })
    );
}

/// The renderer refuses a track it loaded itself on the name the failure carries.
#[test]
fn a_load_failure_of_this_host_names_the_track() {
    let event = PlayerBridgeEvent::media_error(
        "HTTP 403 Forbidden",
        "no_such_file",
        Some("120002099"),
        LoadOrigin::Local,
    );
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        serde_json::json!({
            "t": "mediaerror",
            "v": { "error": "HTTP 403 Forbidden", "errorCode": "no_such_file", "productId": "120002099" }
        })
    );
}

/// A load a controller made is the receiver's to report: naming its track would let the page
/// refuse the one it loaded itself when both are the same product.
#[test]
fn a_load_failure_during_a_cast_names_no_track() {
    let event = PlayerBridgeEvent::media_error(
        "HTTP 403 Forbidden",
        "no_such_file",
        Some("120002099"),
        LoadOrigin::Connect,
    );
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        serde_json::json!({
            "t": "mediaerror",
            "v": { "error": "HTTP 403 Forbidden", "errorCode": "no_such_file" }
        })
    );
}

/// The counterweight, and the shape TIDAL's native player already reads: a failure with no
/// track to name is the payload it always was.
#[test]
fn a_failure_that_names_no_track_is_unchanged() {
    let event =
        PlayerBridgeEvent::media_error("probe failed", "unreadable_file", None, LoadOrigin::Local);
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        serde_json::json!({
            "t": "mediaerror",
            "v": { "error": "probe failed", "errorCode": "unreadable_file" }
        })
    );
}
