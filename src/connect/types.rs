use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::AtomicU32;
use tokio::sync::oneshot;

// ── Device (mDNS discovery) ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MdnsDevice {
    pub addresses: Vec<String>,
    pub friendly_name: String,
    pub fullname: String,
    pub id: String,
    pub port: u16,
    #[serde(rename = "type")]
    pub device_type: DeviceType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DeviceType {
    TidalConnect,
}

// ── Session ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionStatus {
    Active,
    Suspended,
}

// Session commands (controller → device)
#[derive(Serialize, Debug)]
#[serde(tag = "command", rename_all = "camelCase")]
#[allow(clippy::enum_variant_names)]
pub(crate) enum SessionCommand {
    #[serde(rename = "startSession")]
    StartSession {
        #[serde(rename = "appId")]
        app_id: String,
        #[serde(rename = "appName")]
        app_name: String,
        #[serde(rename = "sessionCredential", skip_serializing_if = "Option::is_none")]
        session_credential: Option<String>,
    },
    #[serde(rename = "resumeSession")]
    ResumeSession {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    #[serde(rename = "endSession")]
    EndSession {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "stopCasting")]
        stop_casting: bool,
    },
}

// Session notifications (device → controller)
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "command", rename_all = "camelCase")]
#[allow(clippy::enum_variant_names)]
pub(crate) enum SessionNotification {
    #[serde(rename = "notifySessionStarted")]
    NotifySessionStarted {
        #[serde(rename = "sessionId")]
        session_id: String,
        joined: bool,
    },
    #[serde(rename = "notifySessionResumed")]
    NotifySessionResumed {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    #[serde(rename = "notifySessionEnded")]
    NotifySessionEnded {
        #[serde(rename = "sessionId")]
        session_id: String,
        suspended: bool,
    },
    #[serde(rename = "notifySessionError")]
    NotifySessionError {
        #[serde(rename = "requestedCommand")]
        requested_command: String,
        #[serde(flatten)]
        details: serde_json::Value,
    },
    #[serde(rename = "notifySessionState")]
    NotifySessionState { state: serde_json::Value },
}

// ── Playback ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PbState {
    NoEngine,
    Preparing,
    Started,
    Completed,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PbPlayState {
    Playing,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PlayerState {
    #[serde(rename = "IDLE", alias = "idle")]
    Idle,
    #[serde(rename = "BUFFERING", alias = "buffering")]
    Buffering,
    #[serde(rename = "PAUSED", alias = "paused")]
    Paused,
    #[serde(rename = "PLAYING", alias = "playing")]
    Playing,
}

// For reference - the PlayerState serialization discussion:
// The real binary SDK uses lowercase ("playing", "paused") but the TIDAL desktop
// TypeScript client (mediaManager.js) uses uppercase ("PLAYING", "PAUSED").
// The mobile TIDAL app accepts both (confirmed: play/pause works with uppercase).
// We use uppercase with lowercase aliases for deserialization compatibility.

// Playback notifications (device/receiver → controller/mobile)
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "command", rename_all = "camelCase")]
#[allow(clippy::enum_variant_names)]
pub(crate) enum PlaybackNotification {
    #[serde(rename = "notifyPlayerStatusChanged")]
    NotifyPlayerStatusChanged {
        #[serde(rename = "playerState")]
        player_state: PlayerState,
        progress: u64,
    },
    #[serde(rename = "notifyMediaChanged")]
    NotifyMediaChanged {
        #[serde(rename = "mediaInfo")]
        media_info: MediaInfo,
    },
    #[serde(rename = "notifyPlaybackError")]
    NotifyPlaybackError {
        #[serde(rename = "errorCode", skip_serializing_if = "Option::is_none")]
        error_code: Option<u32>,
        #[serde(rename = "statusCode", skip_serializing_if = "Option::is_none")]
        status_code: Option<String>,
    },
    #[serde(rename = "notifyRequestResult")]
    NotifyRequestResult {
        #[serde(rename = "resultCode")]
        result_code: u32,
        #[serde(rename = "subCode", skip_serializing_if = "Option::is_none")]
        sub_code: Option<u32>,
    },
    #[serde(rename = "notifyAudioFormatUpdated")]
    NotifyAudioFormatUpdated {
        #[serde(rename = "audioBitrate", skip_serializing_if = "Option::is_none")]
        audio_bitrate: Option<u32>,
        #[serde(rename = "audioChannels", skip_serializing_if = "Option::is_none")]
        audio_channels: Option<u32>,
        #[serde(rename = "audioBitPerSample", skip_serializing_if = "Option::is_none")]
        audio_bit_per_sample: Option<u32>,
        #[serde(rename = "audioSamplingRate", skip_serializing_if = "Option::is_none")]
        audio_sampling_rate: Option<u32>,
        #[serde(rename = "audioCodec", skip_serializing_if = "Option::is_none")]
        audio_codec: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mediaformat: Option<String>,
        #[serde(rename = "streamType", skip_serializing_if = "Option::is_none")]
        stream_type: Option<String>,
    },
}

// ── Media ────────────────────────────────────────────────────────────

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

// ── Queue ────────────────────────────────────────────────────────────

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

// ── Server info (OAuth + content/queue) ──────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServerInfo {
    pub server_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_info: Option<AuthInfo>,
    #[serde(default)]
    pub http_header_fields: Vec<String>,
    #[serde(default)]
    pub query_parameters: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuthInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_auth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_server_info: Option<OAuthServerInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_parameters: Option<OAuthParameters>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OAuthServerInfo {
    pub server_url: String,
    pub auth_info: OAuthAuthInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form_parameters: Option<OAuthFormParameters>,
    #[serde(default)]
    pub http_header_fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OAuthAuthInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_auth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_parameters: Option<OAuthParameters>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OAuthParameters {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct OAuthFormParameters {
    pub grant_type: String,
    pub scope: String,
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

// ── Receiver config ──────────────────────────────────────────────────

pub(crate) struct ReceiverConfig {
    pub ws_port: u16,
    pub friendly_name: String,
    pub model_name: String,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            ws_port: consts::WS_DEFAULT_PORT,
            friendly_name: format!(
                "TidaLunar: {}",
                gethostname::gethostname().to_string_lossy()
            ),
            model_name: "TidaLunar".to_string(),
        }
    }
}

// ── Pending request registry ─────────────────────────────────────────

pub(crate) struct PendingRequests {
    next_id: AtomicU32,
    pending: std::sync::Mutex<HashMap<u32, oneshot::Sender<serde_json::Value>>>,
}

impl PendingRequests {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU32::new(0),
            pending: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn register(&self) -> (u32, oneshot::Receiver<serde_json::Value>) {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    pub fn resolve(&self, request_id: u32, response: serde_json::Value) -> bool {
        if let Some(tx) = self.pending.lock().unwrap().remove(&request_id) {
            tx.send(response).is_ok()
        } else {
            false
        }
    }

    pub fn remove(&self, request_id: u32) {
        self.pending.lock().unwrap().remove(&request_id);
    }

    pub fn fail_all(&self) {
        self.pending.lock().unwrap().clear();
    }
}

// ── Constants ────────────────────────────────────────────────────────

pub(crate) mod consts {
    // mDNS
    pub const MDNS_SERVICE_TYPE: &str = "_tidalconnect._tcp.local.";

    // WebSocket
    pub const WS_DEFAULT_PORT: u16 = 9000;
    pub const PING_INTERVAL_MS: u64 = 15_000;
    pub const PING_TIMEOUT_MS: u64 = 31_000;

    // Session
    pub const SESSION_APP_ID: &str = "tidal";
    pub const SESSION_APP_NAME: &str = "tidal";

    // HTTP
    pub const HTTP_TIMEOUT_SECS: u64 = 120;
}
