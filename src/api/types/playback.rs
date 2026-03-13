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

/// A single audio representation inside a DASH manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashAudio {
    #[serde(default)]
    pub bitrate: Option<DashBitrate>,
    #[serde(default)]
    pub size: Option<DashSize>,
}

/// Bitrate info for a DASH audio representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashBitrate {
    #[serde(default)]
    pub bps: Option<u64>,
}

/// Size info for a DASH audio representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashSize {
    #[serde(default)]
    pub b: Option<u64>,
}

/// Decoded DASH manifest (simplified representation).
///
/// The full DASH MPD is XML, but for our purposes we only care about
/// the audio track list extracted from it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashManifest {
    pub tracks: DashTracks,
}

/// Track container in a DASH manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashTracks {
    #[serde(default)]
    pub audios: Vec<DashAudio>,
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
