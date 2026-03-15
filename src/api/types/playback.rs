use serde::{Deserialize, Serialize};

use super::quality::{AudioMode, AudioQuality, ManifestMimeType};

/// Raw playback info as returned by
/// `GET /v1/tracks/{id}/playbackinfo?audioquality=...&playbackmode=STREAM&assetpresentation=FULL`.
///
/// The `manifest` field is a **base64-encoded** string. Depending on
/// `manifest_mime_type` it decodes to either JSON ([`BtsManifest`]) or
/// DASH XML.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackInfoResponse {
    pub track_id: u64,
    pub asset_presentation: String,
    pub audio_mode: AudioMode,
    pub audio_quality: AudioQuality,
    pub manifest_mime_type: ManifestMimeType,
    pub manifest_hash: String,
    /// Base64-encoded manifest payload.
    pub manifest: String,

    // Replay gain / normalization
    pub album_replay_gain: f64,
    pub album_peak_amplitude: f64,
    pub track_replay_gain: f64,
    pub track_peak_amplitude: f64,

    // Format details
    #[serde(default)]
    pub bit_depth: Option<u32>,
    #[serde(default)]
    pub sample_rate: Option<u32>,
}

/// Decoded BTS manifest (from base64 JSON when
/// `manifestMimeType` = `application/vnd.tidal.bts`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BtsManifest {
    pub mime_type: String,
    pub codecs: String,
    #[serde(default)]
    pub encryption_type: Option<String>,
    pub urls: Vec<String>,
    #[serde(default)]
    pub key_id: Option<String>,
}

/// Decoded DASH manifest with segment URLs extracted from MPD XML.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashManifest {
    pub init_url: String,
    pub segment_urls: Vec<String>,
    pub codec: String,
    #[serde(default)]
    pub sample_rate: Option<u32>,
    #[serde(default)]
    pub bandwidth: Option<u32>,
}

/// Processed playback info with a decoded manifest.
///
/// This is the high-level type returned to callers after base64
/// decoding + JSON/XML parsing of the raw manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackInfo {
    pub track_id: u64,
    pub asset_presentation: String,
    pub audio_mode: AudioMode,
    pub audio_quality: AudioQuality,
    pub manifest_mime_type: ManifestMimeType,

    pub album_replay_gain: f64,
    pub album_peak_amplitude: f64,
    pub track_replay_gain: f64,
    pub track_peak_amplitude: f64,

    #[serde(default)]
    pub bit_depth: Option<u32>,
    #[serde(default)]
    pub sample_rate: Option<u32>,

    /// MIME type of the audio stream (e.g. `audio/flac`, `audio/mp4`).
    #[serde(default)]
    pub mime_type: Option<String>,

    /// Decoded manifest — either BTS or DASH.
    pub decoded_manifest: DecodedManifest,
}

/// Union of possible decoded manifest types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DecodedManifest {
    #[serde(rename = "bts")]
    Bts(BtsManifest),
    #[serde(rename = "dash")]
    Dash(DashManifest),
}
