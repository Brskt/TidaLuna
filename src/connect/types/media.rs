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
}

impl MediaInfo {
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
