//! Tests for `src/connect/receiver/queue/media.rs`, attached to it by `#[path]`.
//!
//! Only the queue-item to `MediaInfo` transform is exercised. It is pure, and the length it
//! resolves is the only one a connected controller is ever told for a track.

use super::queue_item_to_media_info;
use crate::connect::types::QueueItem;
use serde_json::json;

/// A window item as the wire sends it, carrying only what the test is about beside its ids.
fn item(fields: serde_json::Value) -> QueueItem {
    let mut wire = json!({ "item_id": "i-1", "media_id": "88264189" });
    let object = wire.as_object_mut().expect("built as an object");
    for (key, value) in fields.as_object().expect("fields is an object") {
        object.insert(key.clone(), value.clone());
    }
    serde_json::from_value(wire).expect("the two ids are all the wire requires")
}

fn resolved_duration(fields: serde_json::Value) -> Option<u64> {
    queue_item_to_media_info(&item(fields))
        .metadata
        .and_then(|m| m.duration)
}

#[test]
fn a_length_survives_an_item_that_carries_no_display_info() {
    // The defect: `duration_ms` sits at the item's own top level, but its only reader was an
    // `.or()` inside the `display_info` branch. A window item carrying a length and no display
    // info reached the controller with none at all, for as long as the track played.
    assert_eq!(
        resolved_duration(json!({ "duration_ms": 214_000 })),
        Some(214_000),
        "the item's own length was dropped along with display info it never carried"
    );
}

#[test]
fn display_info_still_wins_over_the_items_own_length() {
    assert_eq!(
        resolved_duration(json!({
            "duration_ms": 1,
            "display_info": { "duration": 214_000 },
        })),
        Some(214_000),
        "the richer source stopped being the preferred one"
    );
}

#[test]
fn the_items_own_length_backs_a_display_info_that_omits_one() {
    assert_eq!(
        resolved_duration(json!({
            "duration_ms": 214_000,
            "display_info": { "title": "Automatic" },
        })),
        Some(214_000)
    );
}

#[test]
fn an_item_naming_no_length_anywhere_mints_none() {
    // Nothing is invented: absent stays absent rather than becoming zero, which `set_metadata`
    // writes as a zero-second track rather than an unknown one.
    assert_eq!(resolved_duration(json!({})), None);
}
