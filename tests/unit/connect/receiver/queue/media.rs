//! Tests for `src/connect/receiver/queue/media.rs`, attached to it by `#[path]`.
//!
//! Only the queue-item to `MediaInfo` transform is exercised. It is pure, and the length it
//! resolves is the only one a connected controller is ever told for a track.

use super::queue_item_to_media_info;
use crate::connect::types::{QueueItem, ResolvedStream};
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

fn playback_info(fields: serde_json::Value) -> ResolvedStream {
    ResolvedStream::from_playbackinfo(&fields)
}

#[test]
fn the_stream_is_what_the_playbackinfo_answer_names_not_what_was_asked() {
    // The controller asks `HI_RES_LOSSLESS`; the answer names what came back, and that is what plays.
    let stream = playback_info(json!({
        "audioQuality": "LOSSLESS",
        "bitDepth": 16,
        "sampleRate": 44_100,
        "manifestMimeType": "application/vnd.tidal.bts",
    }));
    assert_eq!(stream.audio_quality.as_deref(), Some("LOSSLESS"));
    assert_eq!(stream.bit_depth, Some(16));
    assert_eq!(stream.sample_rate, Some(44_100));
}

#[test]
fn a_field_the_answer_omits_or_mistypes_stays_absent() {
    // A lossy tier carries no bit depth; nothing is invented, and a value that does not fit stays out.
    let stream =
        playback_info(json!({ "audioQuality": "HIGH", "bitDepth": null, "sampleRate": "44100" }));
    assert_eq!(stream.audio_quality.as_deref(), Some("HIGH"));
    assert_eq!(stream.bit_depth, None);
    assert_eq!(stream.sample_rate, None);
    assert_eq!(
        playback_info(json!({ "sampleRate": 5_000_000_000_u64 })).sample_rate,
        None
    );
    assert_eq!(playback_info(json!({})), ResolvedStream::default());
}

#[test]
fn the_page_hears_the_stream_and_the_controller_never_does() {
    let mut media = queue_item_to_media_info(&item(json!({})));
    media.stream = Some(playback_info(
        json!({ "audioQuality": "HIGH", "sampleRate": 48_000 }),
    ));

    let page = media.page_payload();
    assert_eq!(page["mediaId"], "88264189");
    assert_eq!(page["stream"]["audioQuality"], "HIGH");
    assert_eq!(page["stream"]["sampleRate"], 48_000);
    assert!(page["stream"]["bitDepth"].is_null());

    let wire = serde_json::to_value(&media).expect("a MediaInfo serializes");
    assert!(
        wire.get("stream").is_none(),
        "the controller's message grew a field it never had"
    );
}

#[test]
fn a_track_with_no_resolved_stream_announces_null_not_a_neighbours_format() {
    let media = queue_item_to_media_info(&item(json!({})));
    assert!(media.page_payload()["stream"].is_null());
}
