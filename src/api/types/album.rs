use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::artist::ArtistInline;
use super::quality::{AlbumType, AudioMode, AudioQuality, MediaMetadata};
use super::track::MediaItemWrapper;

/// Full album object as returned by `GET /v1/albums/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Album {
    pub id: Value,
    pub title: String,
    #[serde(default)]
    pub version: Option<String>,
    pub duration: u32,
    pub number_of_tracks: u32,
    #[serde(default)]
    pub number_of_videos: Option<u32>,
    #[serde(default)]
    pub number_of_volumes: Option<u32>,
    #[serde(default)]
    pub cover: Option<String>,
    #[serde(default)]
    pub video_cover: Option<String>,
    #[serde(default)]
    pub vibrant_color: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub stream_start_date: Option<String>,
    #[serde(default)]
    pub copyright: Option<String>,
    #[serde(default)]
    pub r#type: Option<AlbumType>,
    #[serde(default)]
    pub url: Option<String>,
    pub explicit: bool,
    #[serde(default)]
    pub upc: Option<String>,
    #[serde(default)]
    pub popularity: Option<u32>,

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
    pub premium_streaming_only: Option<bool>,

    // Relations
    #[serde(default)]
    pub artist: Option<ArtistInline>,
    #[serde(default)]
    pub artists: Option<Vec<ArtistInline>>,

    // Extra
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub record_label: Option<String>,
}

/// A module inside an album page response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumPageModule {
    pub r#type: String,
    #[serde(default)]
    pub paged_list: Option<AlbumPagedList>,
}

/// Paged list of media items within an album page module.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumPagedList {
    pub items: Vec<MediaItemWrapper>,
}

/// A row in an album page response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumPageRow {
    pub modules: Vec<AlbumPageModule>,
}

/// Album page response from `GET /v1/pages/album`.
///
/// Used to retrieve the full track listing for an album. The actual
/// items live inside the module whose `type` is `"ALBUM_ITEMS"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumPage {
    pub id: String,
    pub title: String,
    pub rows: Vec<AlbumPageRow>,
}
