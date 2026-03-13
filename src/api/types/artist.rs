use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::quality::ArtistRole;

/// Compact artist reference embedded in track/album responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistInline {
    pub id: Value,
    pub name: String,
    #[serde(default)]
    pub r#type: Option<ArtistRole>,
    #[serde(default)]
    pub picture: Option<String>,
}

/// Artist role category (from artist detail / credits endpoints).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistRoleCategory {
    pub category_id: u32,
    pub category: String,
}

/// Full artist object as returned by `GET /v1/artists/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artist {
    pub id: Value,
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub picture: Option<String>,
    #[serde(default)]
    pub popularity: Option<u32>,
    #[serde(default)]
    pub artist_types: Option<Vec<String>>,
    #[serde(default)]
    pub artist_roles: Option<Vec<ArtistRoleCategory>>,
    #[serde(default)]
    pub mixes: Option<Value>,
}
