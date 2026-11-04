//! A device frame as TIDAL's controller hands it to the page.
//!
//! The device sends two fields as JSON text that TIDAL's native layer parses before the page reads
//! them (`TidalConnectController.js`, `handleDeviceMessage`): a server error's `details.description`
//! and a media change's `mediaInfo.customData`. The page's sagas read both as objects.

use serde_json::{Map, Value};

/// The two fields a device sends as JSON text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeviceText {
    /// A server error's `details.description`.
    Description,
    /// A media change's `mediaInfo.customData`.
    CustomData,
}

/// What normalizing a frame did to its text field.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TextField {
    /// No text to parse: none in the frame, a value that is not text (kept as sent), or an empty
    /// `customData` (dropped, as TIDAL reads it as absent).
    Untouched,
    Parsed,
    /// The text is not JSON and was dropped from the frame. Describes the fault without the
    /// text, which can carry a server's response body.
    NotJson(String),
}

/// `frame` with those two fields parsed, once, where the controller takes a frame in: the page's
/// push and the state snapshot then read the same value. A field that is not JSON is dropped
/// from the frame, and the caller decides what that costs it. The field is `None` for a frame
/// that carries neither.
pub(crate) fn normalize_device_frame(mut frame: Value) -> (Value, Option<(DeviceText, TextField)>) {
    let command = frame
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let field = match command.as_str() {
        "notifyContentServerError" | "notifyQueueServerError" => {
            let error_code = frame.get("errorCode").and_then(Value::as_u64);
            // TIDAL parses a `description` that is text, the empty one included, and throws.
            frame
                .get_mut("details")
                .and_then(Value::as_object_mut)
                .map(|details| {
                    (
                        DeviceText::Description,
                        parse_text_field(details, "description", false, |what| {
                            let code =
                                error_code.map_or_else(|| "none".to_owned(), |c| c.to_string());
                            format!("{command} (errorCode {code}): description {what}")
                        }),
                    )
                })
        }
        // TIDAL's `customData ? JSON.parse(...) : undefined`: the empty text names nothing.
        "notifyMediaChanged" => frame
            .get_mut("mediaInfo")
            .and_then(Value::as_object_mut)
            .map(|media| {
                (
                    DeviceText::CustomData,
                    parse_text_field(media, "customData", true, |what| {
                        format!("{command}: customData {what}")
                    }),
                )
            }),
        _ => None,
    };
    (frame, field)
}

/// Replaces the text under `key` with the JSON it holds. `empty_is_absent` tells whether an empty
/// text names nothing or is the invalid JSON it is to `JSON.parse`.
fn parse_text_field(
    object: &mut Map<String, Value>,
    key: &str,
    empty_is_absent: bool,
    fault: impl FnOnce(&str) -> String,
) -> TextField {
    // A value that is not text stays as the device sent it, as TIDAL's own `typeof` test leaves it.
    if !object.get(key).is_some_and(Value::is_string) {
        return TextField::Untouched;
    }
    let Some(Value::String(text)) = object.remove(key) else {
        return TextField::Untouched;
    };
    if text.is_empty() && empty_is_absent {
        return TextField::Untouched;
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(parsed) => {
            object.insert(key.to_owned(), parsed);
            TextField::Parsed
        }
        Err(e) => TextField::NotJson(fault(&format!(
            "is not JSON ({:?} at {}:{})",
            e.classify(),
            e.line(),
            e.column()
        ))),
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/connect/types/device_frame.rs"]
mod tests;
