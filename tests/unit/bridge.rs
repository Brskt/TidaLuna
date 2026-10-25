//! Tests for `src/bridge.rs`, attached to it by `#[path]`.
//!
//! The renderer's queue reads a state event's `cast` field before acting on a completion. Its
//! self-load flag names the last load that page issued, which a Connect load never touches: a
//! stale flag advanced the local queue over the cast at every track boundary.

use super::PlayerBridgeEvent;
use crate::player::LoadOrigin;

/// This host's own states carry no `cast` field, the shape every renderer already parses; one
/// handed over while a controller owns the player says so.
#[test]
fn a_state_carries_cast_only_while_a_controller_owns_the_player() {
    for (owner, expected) in [
        (
            LoadOrigin::Connect,
            serde_json::json!({ "t": "state", "seq": 7, "v": "completed", "cast": true }),
        ),
        (
            LoadOrigin::Local,
            serde_json::json!({ "t": "state", "seq": 7, "v": "completed" }),
        ),
    ] {
        let event = PlayerBridgeEvent::state("completed", 7, owner);
        assert_eq!(serde_json::to_value(&event).unwrap(), expected, "{owner:?}");
    }
}

/// A refused load mints no generation, so its answer carries no seq: one would outrank the
/// cast's own events in the renderer, which drops every event older than the newest it saw.
#[test]
fn the_refusal_answer_carries_no_seq_and_names_the_load_it_answers() {
    for (load, expected) in [
        (
            None,
            serde_json::json!({ "t": "mediacastowned", "v": null }),
        ),
        (
            Some(7),
            serde_json::json!({ "t": "mediacastowned", "v": { "load": 7 } }),
        ),
    ] {
        assert_eq!(
            serde_json::to_value(PlayerBridgeEvent::cast_owns_player(load)).unwrap(),
            expected,
            "{load:?}"
        );
    }
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

/// The renderer refuses a track it loaded itself on the name the failure carries. A load a
/// controller made is the receiver's to report: naming its track would let the page refuse the one
/// it loaded itself. With no track to name, the payload is the one TIDAL's native player reads.
#[test]
fn a_load_failure_carries_a_product_only_for_a_named_track_this_host_loaded() {
    for (error, code, product_id, owner, expected) in [
        (
            "HTTP 403 Forbidden",
            "no_such_file",
            Some("120002099"),
            LoadOrigin::Local,
            serde_json::json!({
                "t": "mediaerror",
                "v": { "error": "HTTP 403 Forbidden", "errorCode": "no_such_file", "productId": "120002099" }
            }),
        ),
        (
            "HTTP 403 Forbidden",
            "no_such_file",
            Some("120002099"),
            LoadOrigin::Connect,
            serde_json::json!({
                "t": "mediaerror",
                "v": { "error": "HTTP 403 Forbidden", "errorCode": "no_such_file" }
            }),
        ),
        (
            "probe failed",
            "unreadable_file",
            None,
            LoadOrigin::Local,
            serde_json::json!({
                "t": "mediaerror",
                "v": { "error": "probe failed", "errorCode": "unreadable_file" }
            }),
        ),
    ] {
        let event = PlayerBridgeEvent::media_error(error, code, product_id, owner);
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            expected,
            "{owner:?}, {product_id:?}"
        );
    }
}

/// The renderer answers a media element with the length of the track the element holds (the
/// length names the track it was measured on); a load that carried no id has none to name.
#[test]
fn a_duration_names_the_track_it_was_measured_on_when_it_has_one() {
    for (product, expected) in [
        (
            Some("120002099".to_owned()),
            serde_json::json!({ "t": "duration", "seq": 7, "v": 215.0, "product": "120002099" }),
        ),
        (
            None,
            serde_json::json!({ "t": "duration", "seq": 7, "v": 215.0 }),
        ),
    ] {
        let event = PlayerBridgeEvent::duration(215.0, 7, product.clone());
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            expected,
            "{product:?}"
        );
    }
}
