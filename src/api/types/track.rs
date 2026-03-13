use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::artist::ArtistInline;
use super::quality::{AudioMode, AudioQuality, MediaMetadata};

/// Album reference inlined in a track response.
///
/// This is the compact form — only `id`, `title`, and `cover` are
/// guaranteed. Use the full `Album` struct for `/v1/albums/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumInline {
    pub id: Value,
    pub title: String,
    #[serde(default)]
    pub cover: Option<String>,
    #[serde(default)]
    pub video_cover: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
}

/// Full track object as returned by `GET /v1/tracks/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub id: Value,
    pub title: String,
    #[serde(default)]
    pub version: Option<String>,
    pub duration: u32,
    pub track_number: u32,
    pub volume_number: u32,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub isrc: Option<String>,
    pub explicit: bool,
    #[serde(default)]
    pub popularity: Option<u32>,
    #[serde(default)]
    pub copyright: Option<String>,
    #[serde(default)]
    pub bpm: Option<u32>,

    // Replay gain
    #[serde(default)]
    pub replay_gain: Option<f64>,
    #[serde(default)]
    pub peak: Option<f64>,

    // Quality
    #[serde(default)]
    pub audio_quality: Option<AudioQuality>,
    #[serde(default)]
    pub audio_modes: Option<Vec<AudioMode>>,
    #[serde(default)]
    pub media_metadata: Option<MediaMetadata>,

    // Streaming flags
    #[serde(default)]
    pub allow_streaming: Option<bool>,
    #[serde(default)]
    pub stream_ready: Option<bool>,
    #[serde(default)]
    pub stream_start_date: Option<String>,
    #[serde(default)]
    pub premium_streaming_only: Option<bool>,

    // Relations
    #[serde(default)]
    pub artist: Option<ArtistInline>,
    #[serde(default)]
    pub artists: Option<Vec<ArtistInline>>,
    #[serde(default)]
    pub album: Option<AlbumInline>,

    #[serde(default)]
    pub editable: Option<bool>,
    #[serde(default)]
    pub mixes: Option<Value>,
}

/// Wrapper for media items returned in lists (playlists, album items).
///
/// The TIDAL API wraps each item in `{ "item": Track|Video, "type": "track"|"video" }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaItemWrapper {
    pub item: Value,
    pub r#type: String,
    #[serde(default)]
    pub cut: Option<Value>,
}
