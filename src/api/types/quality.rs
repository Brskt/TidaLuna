use serde::{Deserialize, Serialize};

/// Audio quality tier as returned by the TIDAL API.
///
/// Ordered from lowest to highest fidelity. The `u8` repr allows
/// direct numeric comparison (`HiResLossless > Lossless`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum AudioQuality {
    #[serde(rename = "LOW")]
    Low,
    #[serde(rename = "HIGH")]
    High,
    #[serde(rename = "LOSSLESS")]
    Lossless,
    #[serde(rename = "HI_RES")]
    HiRes,
    #[serde(rename = "HI_RES_LOSSLESS")]
    HiResLossless,
}

impl AudioQuality {
    /// Return the TIDAL API string for this quality tier.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "LOW",
            Self::High => "HIGH",
            Self::Lossless => "LOSSLESS",
            Self::HiRes => "HI_RES",
            Self::HiResLossless => "HI_RES_LOSSLESS",
        }
    }
}

/// Audio playback mode / spatial format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AudioMode {
    #[serde(rename = "STEREO")]
    Stereo,
    #[serde(rename = "DOLBY_ATMOS")]
    DolbyAtmos,
    #[serde(rename = "SONY_360RA")]
    Sony360Ra,
}

/// Tag surfaced in `mediaMetadata.tags` on tracks and albums.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MediaMetadataTag {
    #[serde(rename = "LOSSLESS")]
    Lossless,
    #[serde(rename = "HIRES_LOSSLESS")]
    HiResLossless,
    #[serde(rename = "DOLBY_ATMOS")]
    DolbyAtmos,
    #[serde(rename = "SONY_360RA")]
    Sony360Ra,
    #[serde(rename = "MQA")]
    Mqa,
}

/// Wrapper for the `mediaMetadata` object on tracks/albums.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaMetadata {
    #[serde(default)]
    pub tags: Vec<MediaMetadataTag>,
}

/// Manifest MIME type discriminant for playback info.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ManifestMimeType {
    #[serde(rename = "application/vnd.tidal.bts")]
    Bts,
    #[serde(rename = "application/dash+xml")]
    Dash,
}

/// Album release type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AlbumType {
    Album,
    Ep,
    Single,
    Compilation,
}

/// Playlist ownership type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlaylistType {
    Editorial,
    Artist,
    User,
}

/// Artist role in the context of a track or album.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ArtistRole {
    Main,
    Featured,
    Contributor,
    Composer,
    Producer,
    Artist,
}
