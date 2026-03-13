use serde::{Deserialize, Serialize};

/// Lyrics response from `GET /v1/tracks/{id}/lyrics`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lyrics {
    pub track_id: u64,
    #[serde(default)]
    pub lyrics_provider: Option<String>,
    #[serde(default)]
    pub provider_commontrack_id: Option<String>,
    #[serde(default)]
    pub provider_lyrics_id: Option<String>,
    /// Full lyrics text (plain, un-synced).
    #[serde(default)]
    pub lyrics: Option<String>,
    /// Synced lyrics in LRC format (`[MM:SS.ms] line`).
    #[serde(default)]
    pub subtitles: Option<String>,
    #[serde(default)]
    pub is_right_to_left: Option<bool>,
}
