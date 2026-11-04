//! Tests for `src/connect/types/device_frame.rs`, attached to it by `#[path]`.

use serde_json::json;

use super::{DeviceText, TextField, normalize_device_frame};

/// TIDAL's page reads `description` as an object, and `'subStatus' in "..."` throws on a string
/// with nothing shown: the refresh a subStatus 11001-11003 asks for never started.
#[test]
fn a_server_errors_description_is_normalized_to_the_object_it_holds() {
    for command in ["notifyContentServerError", "notifyQueueServerError"] {
        let frame = json!({
            "command": command,
            "errorCode": 401,
            "details": {
                "description": r#"{"status":401,"subStatus":11003}"#,
                "subCode": 3,
            },
        });
        let (out, field) = normalize_device_frame(frame);
        assert_eq!(
            field,
            Some((DeviceText::Description, TextField::Parsed)),
            "{command}"
        );
        assert_eq!(
            out["details"]["description"]["subStatus"], 11003,
            "{command}"
        );
        assert_eq!(out["details"]["subCode"], 3);
        assert_eq!(out["errorCode"], 401);
    }
}

/// A description that is not JSON (the empty text included, which `JSON.parse` rejects too) is
/// reported as such, without its content: a server's response body can end up in it.
#[test]
fn a_description_that_is_not_json_is_described_without_its_content() {
    for text in ["secret-body {", ""] {
        let frame = json!({
            "command": "notifyQueueServerError",
            "errorCode": 401,
            "details": { "description": text, "subCode": 3 },
        });
        let (out, field) = normalize_device_frame(frame);
        assert!(out["details"].get("description").is_none(), "{text:?}");
        assert_eq!(out["details"]["subCode"], 3);

        let Some((DeviceText::Description, TextField::NotJson(fault))) = field else {
            panic!("{text:?} is not JSON");
        };
        assert!(fault.contains("notifyQueueServerError") && fault.contains("401"));
        assert!(!fault.contains("secret-body"), "{fault}");
    }
}

/// `customData` is where TIDAL's page reads the quality a track plays at: left as text it falls
/// back to LOW with no error.
#[test]
fn a_tracks_custom_data_is_normalized_to_parsed_or_dropped() {
    let frame = json!({
        "command": "notifyMediaChanged",
        "mediaInfo": {
            "mediaId": "1",
            "customData": r#"{"audioQuality":"HI_RES_LOSSLESS","bitDepth":24}"#,
        },
    });
    let (out, field) = normalize_device_frame(frame);
    assert_eq!(field, Some((DeviceText::CustomData, TextField::Parsed)));
    assert_eq!(
        out["mediaInfo"]["customData"]["audioQuality"],
        "HI_RES_LOSSLESS"
    );
    assert_eq!(out["mediaInfo"]["mediaId"], "1");

    // Empty names nothing; bad text is dropped, the track still announced.
    let (out, field) = normalize_device_frame(json!({
        "command": "notifyMediaChanged",
        "mediaInfo": { "mediaId": "1", "customData": "" },
    }));
    assert_eq!(field, Some((DeviceText::CustomData, TextField::Untouched)));
    assert!(out["mediaInfo"].get("customData").is_none());

    let (out, field) = normalize_device_frame(json!({
        "command": "notifyMediaChanged",
        "mediaInfo": { "mediaId": "1", "customData": "{not json" },
    }));
    assert!(matches!(
        field,
        Some((DeviceText::CustomData, TextField::NotJson(_)))
    ));
    assert!(out["mediaInfo"].get("customData").is_none());
    assert_eq!(out["mediaInfo"]["mediaId"], "1");
}
