use anyhow::Result;

use super::client::{API_BASE, fetch_json, query_args};
use super::types::artist::Artist;

/// Fetch a single artist by ID.
///
/// `GET /v1/artists/{id}?countryCode=...&deviceType=DESKTOP&locale=...`
pub async fn artist(artist_id: &str) -> Result<Option<Artist>> {
    let url = format!("{API_BASE}/artists/{artist_id}?{}", query_args());
    fetch_json(&url).await
}
