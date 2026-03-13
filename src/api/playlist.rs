use anyhow::Result;

use super::client::{API_BASE, fetch_json, query_args};
use super::types::playlist::{Playlist, PlaylistItems};

/// Fetch a single playlist by UUID.
///
/// `GET /v1/playlists/{uuid}?countryCode=...&deviceType=DESKTOP&locale=...`
pub async fn playlist(playlist_uuid: &str) -> Result<Option<Playlist>> {
    let url = format!("{API_BASE}/playlists/{playlist_uuid}?{}", query_args());
    fetch_json(&url).await
}

/// Fetch all items in a playlist.
///
/// `GET /v1/playlists/{uuid}/items?countryCode=...&deviceType=DESKTOP&locale=...&limit=-1`
pub async fn playlist_items(playlist_uuid: &str) -> Result<Option<PlaylistItems>> {
    let url = format!(
        "{API_BASE}/playlists/{playlist_uuid}/items?{}&limit=-1",
        query_args()
    );
    fetch_json(&url).await
}
