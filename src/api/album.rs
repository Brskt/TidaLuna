use anyhow::Result;

use super::client::{API_BASE, fetch_json, query_args};
use super::types::album::{Album, AlbumPage};
use super::types::track::MediaItemWrapper;

/// Fetch a single album by ID.
///
/// `GET /v1/albums/{id}?countryCode=...&deviceType=DESKTOP&locale=...`
pub async fn album(album_id: &str) -> Result<Option<Album>> {
    let url = format!("{API_BASE}/albums/{album_id}?{}", query_args());
    fetch_json(&url).await
}

/// Fetch the track listing for an album via the pages endpoint.
///
/// Navigates the `AlbumPage` response to find the `ALBUM_ITEMS`
/// module and returns its media items.
///
/// `GET /v1/pages/album?albumId={id}&countryCode=...&locale=...&deviceType=DESKTOP`
pub async fn album_items(album_id: &str) -> Result<Option<Vec<MediaItemWrapper>>> {
    let url = format!("{API_BASE}/pages/album?albumId={album_id}&{}", query_args());
    let page: Option<AlbumPage> = fetch_json(&url).await?;

    let Some(page) = page else {
        return Ok(None);
    };

    for row in &page.rows {
        for module in &row.modules {
            if module.r#type == "ALBUM_ITEMS"
                && let Some(paged_list) = &module.paged_list
            {
                return Ok(Some(paged_list.items.clone()));
            }
        }
    }

    Ok(None)
}
