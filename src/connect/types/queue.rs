use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueueInfo {
    pub queue_id: String,
    pub repeat_mode: RepeatMode,
    pub shuffled: bool,
    pub max_after_size: u32,
    pub max_before_size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum RepeatMode {
    #[serde(alias = "OFF")]
    None,
    One,
    All,
}

// Queue notifications (device/receiver → controller/mobile)
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "command", rename_all = "camelCase")]
#[allow(clippy::enum_variant_names)]
pub(crate) enum QueueNotification {
    #[serde(rename = "notifyQueueChanged")]
    NotifyQueueChanged {
        #[serde(rename = "queueInfo")]
        queue_info: QueueInfo,
    },
    #[serde(rename = "notifyQueueItemsChanged")]
    NotifyQueueItemsChanged {
        #[serde(rename = "queueInfo")]
        queue_info: QueueInfo,
        #[serde(default)]
        reason: Option<QueueChangeReason>,
    },
    #[serde(rename = "notifyContentServerError")]
    NotifyContentServerError {
        #[serde(rename = "errorCode")]
        error_code: u32,
        details: Option<serde_json::Value>,
    },
    #[serde(rename = "notifyQueueServerError")]
    NotifyQueueServerError {
        #[serde(rename = "errorCode")]
        error_code: u32,
        details: Option<serde_json::Value>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum QueueChangeReason {
    RepeatModeChanged,
    ShuffleChanged,
    ItemDeleted,
    RefreshRequested,
}

// ── Queue item (from TIDAL queue API - irregular casing, explicit renames) ─

/// Single item from the queue window response.
/// Fields use snake_case in the TIDAL API (not camelCase), hence explicit renames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct QueueItem {
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default, rename = "src_url")]
    pub src_url: Option<String>,
    #[serde(default, rename = "duration_ms")]
    pub duration_ms: Option<u64>,
    #[serde(alias = "item_id", alias = "id")]
    pub item_id: String,
    #[serde(alias = "media_id")]
    pub media_id: String,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default, rename = "display_info")]
    pub display_info: Option<QueueItemDisplayInfo>,
    #[serde(default)]
    pub properties: Option<serde_json::Value>,
    #[serde(default)]
    pub gapless: Option<bool>,
    #[serde(default, rename = "custom_data")]
    pub custom_data: Option<serde_json::Value>,
    #[serde(default)]
    pub video: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct QueueItemDisplayInfo {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default, alias = "album_title")]
    pub album_title: Option<String>,
    #[serde(default)]
    pub artists: Option<serde_json::Value>,
    #[serde(default)]
    pub images: Option<serde_json::Value>,
    #[serde(default)]
    pub duration: Option<u64>,
}
