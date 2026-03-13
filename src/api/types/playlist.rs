use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::artist::ArtistInline;
use super::quality::PlaylistType;
use super::track::MediaItemWrapper;

/// Creator reference inside a playlist response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistCreator {
    pub id: Value,
}

/// Full playlist object as returned by `GET /v1/playlists/{uuid}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Playlist {
    pub uuid: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub duration: u32,
    pub number_of_tracks: u32,
    #[serde(default)]
    pub number_of_videos: Option<u32>,
    #[serde(default)]
    pub r#type: Option<PlaylistType>,
    #[serde(default)]
    pub public_playlist: Option<bool>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub square_image: Option<String>,
    #[serde(default)]
    pub popularity: Option<u32>,

    // Timestamps
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub last_updated: Option<String>,
    #[serde(default)]
    pub last_item_added_at: Option<String>,

    // Relations
    #[serde(default)]
    pub creator: Option<PlaylistCreator>,
    #[serde(default)]
    pub promoted_artists: Option<Vec<ArtistInline>>,
}

/// Paginated response from `GET /v1/playlists/{uuid}/items`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistItems {
    pub items: Vec<MediaItemWrapper>,
    pub total_number_of_items: u32,
    #[serde(default)]
    pub offset: Option<u32>,
    #[serde(default)]
    pub limit: Option<i32>,
}
