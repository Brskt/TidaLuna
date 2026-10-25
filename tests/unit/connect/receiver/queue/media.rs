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

/// A window item's own `duration_ms` backs a `display_info` that carries none and loses to one that
/// does; absent stays `None`, as zero would be written as a zero-second track. It once sat behind an
/// `.or()` inside the `display_info` branch: an item with a length and no display info lost it.
#[test]
fn an_items_length_is_resolved_from_display_info_first_then_its_own_field() {
    for (fields, expected) in [
        (json!({ "duration_ms": 214_000 }), Some(214_000)),
        (
            json!({ "duration_ms": 1, "display_info": { "duration": 214_000 } }),
            Some(214_000),
        ),
        (
            json!({ "duration_ms": 214_000, "display_info": { "title": "Automatic" } }),
            Some(214_000),
        ),
        (json!({}), None),
    ] {
        assert_eq!(resolved_duration(fields.clone()), expected, "{fields}");
    }
}

fn playback_info(fields: serde_json::Value) -> ResolvedStream {
    ResolvedStream::from_playbackinfo(&fields)
}

/// The controller asks `HI_RES_LOSSLESS`; the answer names what came back, and that plays. A lossy
/// tier carries no bit depth: nothing is invented, and a value that does not fit stays out.
#[test]
fn the_stream_is_what_the_playbackinfo_answer_names_and_nothing_it_omits_or_mistypes() {
    for (answer, expected) in [
        (
            json!({
                "audioQuality": "LOSSLESS",
                "bitDepth": 16,
                "sampleRate": 44_100,
                "manifestMimeType": "application/vnd.tidal.bts",
            }),
            ResolvedStream {
                audio_quality: Some("LOSSLESS".to_string()),
                bit_depth: Some(16),
                sample_rate: Some(44_100),
            },
        ),
        (
            json!({ "audioQuality": "HIGH", "bitDepth": null, "sampleRate": "44100" }),
            ResolvedStream {
                audio_quality: Some("HIGH".to_string()),
                bit_depth: None,
                sample_rate: None,
            },
        ),
        (
            json!({ "sampleRate": 5_000_000_000_u64 }),
            ResolvedStream::default(),
        ),
        (json!({}), ResolvedStream::default()),
    ] {
        assert_eq!(playback_info(answer.clone()), expected, "{answer}");
    }
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
