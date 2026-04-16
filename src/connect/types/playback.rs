use serde::{Deserialize, Serialize};

use super::media::MediaInfo;

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
