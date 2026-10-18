use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MediaInfo {
    pub item_id: String,
    pub media_id: String,
    #[serde(default)]
    pub src_url: Option<String>,
    #[serde(default)]
    pub stream_type: Option<String>,
    #[serde(default)]
    pub metadata: Option<MediaMetadata>,
    #[serde(default)]
    pub custom_data: Option<serde_json::Value>,
    #[serde(default)]
    pub media_type: i32,
    #[serde(default = "default_policy")]
    pub policy: serde_json::Value,
    /// What the `playbackinfo` answer said about the stream this receiver resolved. Never on the
    /// wire to a controller; `page_payload` hands it to the page alone.
    #[serde(skip)]
    pub stream: Option<ResolvedStream>,
}

/// The format of the stream a receiver resolved, as TIDAL's own `playbackinfo` answer names it:
/// the quality the controller asked for is a request, this is what came back and plays. A field
/// the answer omitted stays absent.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResolvedStream {
    pub audio_quality: Option<String>,
    pub bit_depth: Option<u32>,
    pub sample_rate: Option<u32>,
}

impl ResolvedStream {
    pub(crate) fn from_playbackinfo(body: &serde_json::Value) -> Self {
        let number = |key: &str| {
            body.get(key)
                .and_then(serde_json::Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
        };
        Self {
            audio_quality: body
                .get("audioQuality")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            bit_depth: number("bitDepth"),
            sample_rate: number("sampleRate"),
        }
    }
}

impl MediaInfo {
    /// The announcement the local page receives: the track as the controller sees it, plus the
    /// stream resolved for it (`null` when none was, for a source the controller supplied or a
    /// resolution that failed). Both travel in one payload. A format cannot then be matched to
    /// another track than the one it was resolved for.
    pub(crate) fn page_payload(&self) -> serde_json::Value {
        let mut payload = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "stream".to_string(),
                serde_json::to_value(&self.stream).unwrap_or(serde_json::Value::Null),
            );
        }
        payload
    }

    /// The controller's id for this track, absent when it names nothing. The wire field is a
    /// required `String` with no floor of its own; a controller that sends `""` would mint an
    /// id equal to the next blank one and lend one track's length to another.
    pub(crate) fn track_id(&self) -> Option<String> {
        crate::util::metadata::trimmed_non_empty(&self.media_id)
    }
}

pub(crate) fn default_policy() -> serde_json::Value {
    serde_json::json!({"canNext": true, "canPrevious": true})
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MediaMetadata {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default, alias = "albumTitle", alias = "albumName")]
    pub album_title: Option<String>,
    #[serde(default)]
    pub artists: Option<serde_json::Value>,
    #[serde(default)]
    pub duration: Option<u64>,
    #[serde(default)]
    pub images: Option<serde_json::Value>,
}
