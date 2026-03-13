use serde::{Deserialize, Serialize};

/// Paginated response from the TIDAL OpenAPI v2
/// (`GET /v2/tracks?filter[isrc]=...`).
///
/// Follows the JSON:API specification with `data` + `links`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiTracksResponse {
    pub data: Vec<ApiTrack>,
    #[serde(default)]
    pub links: Option<ApiLinks>,
}

/// A single track resource in JSON:API format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiTrack {
    pub id: String,
    pub r#type: String,
    pub attributes: ApiTrackAttributes,
    #[serde(default)]
    pub relationships: Option<ApiTrackRelationships>,
    #[serde(default)]
    pub links: Option<ApiLinks>,
}

/// Track attributes in the OpenAPI v2 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiTrackAttributes {
    pub title: String,
    pub isrc: String,
    /// ISO 8601 duration string (e.g. `"PT2M48S"`).
    pub duration: String,
    #[serde(default)]
    pub copyright: Option<String>,
    #[serde(default)]
    pub explicit: Option<bool>,
    #[serde(default)]
    pub popularity: Option<u32>,
    #[serde(default)]
    pub availability: Option<Vec<String>>,
    /// Quality/spatial tags: `"HIRES_LOSSLESS"`, `"LOSSLESS"`,
    /// `"DOLBY_ATMOS"`, `"SONY_360RA"`, `"MQA"`, etc.
    #[serde(default)]
    pub media_tags: Option<Vec<String>>,
    #[serde(default)]
    pub external_links: Option<Vec<ApiExternalLink>>,
}

/// External link on a v2 track resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiExternalLink {
    pub href: String,
    #[serde(default)]
    pub meta: Option<ApiExternalLinkMeta>,
}

/// Meta on an external link (describes the link type).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiExternalLinkMeta {
    pub r#type: String,
}

/// Relationships on a v2 track resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiTrackRelationships {
    #[serde(default)]
    pub albums: Option<ApiLinksContainer>,
    #[serde(default)]
    pub artists: Option<ApiLinksContainer>,
    #[serde(default)]
    pub similar_tracks: Option<ApiLinksContainer>,
    #[serde(default)]
    pub providers: Option<ApiLinksContainer>,
    #[serde(default)]
    pub radio: Option<ApiLinksContainer>,
}

/// Wrapper containing a `links` object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiLinksContainer {
    #[serde(default)]
    pub links: Option<ApiLinks>,
}

/// Pagination links (JSON:API).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiLinks {
    #[serde(default, rename = "self")]
    pub self_link: Option<String>,
    #[serde(default)]
    pub next: Option<String>,
}
