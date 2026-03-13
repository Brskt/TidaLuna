use serde::{Deserialize, Serialize};

use super::content::format_cover_url;

/// Vorbis comment / FLAC tag set for a track.
///
/// All fields are optional — callers should skip empty fields when
/// writing actual FLAC metadata blocks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlacTags {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disc_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bpm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copyright: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "REPLAYGAIN_TRACK_GAIN"
    )]
    pub replaygain_track_gain: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "REPLAYGAIN_TRACK_PEAK"
    )]
    pub replaygain_track_peak: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub musicbrainz_trackid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub musicbrainz_albumid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genres: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tracks: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lyrics: Option<String>,
}

/// Tags + cover art URL for a track.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetaTags {
    pub tags: FlacTags,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
}

/// Extract artist names from a list, defaulting to `["Unknown Artist"]`.
fn collect_artist_names(artists: Option<&Vec<super::types::artist::ArtistInline>>) -> Vec<String> {
    let names: Vec<String> = artists
        .map(|a| a.iter().map(|x| x.name.clone()).collect())
        .unwrap_or_default();
    if names.is_empty() {
        vec!["Unknown Artist".to_string()]
    } else {
        names
    }
}

/// Parse `YYYY-MM-DD` date and `YYYY` year from an ISO 8601 date string.
fn parse_date_year(s: &str) -> (Option<String>, Option<String>) {
    let date = s.get(..10).map(|d| d.to_string());
    let year = s.get(..4).map(|y| y.to_string());
    (date, year)
}

/// Build FLAC tags for a track by fetching metadata from TIDAL endpoints.
///
/// Mirrors the TypeScript `makeTags(mediaItem)` logic:
/// 1. Fetch track info → title, trackNumber, replayGain, etc.
/// 2. Fetch lyrics → synced subtitles preferred over plain text
/// 3. Fetch album → albumArtist, genre, label, UPC, totalTracks, date
/// 4. Assemble cover URL from album cover UUID
///
/// MusicBrainz lookups are **not** included — they remain in the
/// frontend since they depend on the `musicbrainz-api` npm package
/// and cross-referencing logic tied to the Redux store.
pub async fn make_tags(track_id: &str) -> anyhow::Result<MetaTags> {
    // Fetch track and lyrics concurrently
    let (track_res, lyrics_res) = tokio::join!(
        super::track::track(track_id),
        super::track::lyrics(track_id),
    );

    let track = track_res?;
    let lyrics = lyrics_res?;

    let mut tags = FlacTags::default();
    let mut cover_url: Option<String> = None;

    if let Some(ref t) = track {
        tags.title = Some(t.title.clone());
        tags.track_number = Some(t.track_number.to_string());
        tags.disc_number = Some(t.volume_number.to_string());
        tags.copyright = t.copyright.clone();
        tags.bpm = t.bpm.map(|b| b.to_string());
        tags.isrc = t.isrc.clone();
        tags.comment = t.url.clone();

        tags.replaygain_track_peak = t.peak.map(|p| p.to_string());
        tags.replaygain_track_gain = t.replay_gain.map(|g| g.to_string());

        // Artists from track
        tags.artist = Some(collect_artist_names(t.artists.as_ref()));

        // Date from track streamStartDate (fallback, album date preferred)
        if let Some(ref date_str) = t.stream_start_date {
            let (date, year) = parse_date_year(date_str);
            tags.date = date;
            tags.year = year;
        }

        // Fetch album info if track has an album reference
        if let Some(ref album_inline) = t.album {
            let album_id = album_inline.id.to_string();
            // Strip quotes from JSON string value
            let album_id = album_id.trim_matches('"');

            if let Ok(Some(album)) = super::album::album(album_id).await {
                tags.album = Some(album.title.clone());
                tags.upc = album.upc.clone();
                tags.genres = album.genre.clone();
                tags.organization = album.record_label.clone();
                tags.total_tracks = Some(album.number_of_tracks.to_string());

                // Album date takes precedence over track date
                if let Some(ref rd) = album.release_date {
                    let (date, year) = parse_date_year(rd);
                    tags.date = date;
                    tags.year = year;
                }

                // Album artists
                tags.album_artist = Some(collect_artist_names(album.artists.as_ref()));

                // Cover URL from album
                if let Some(ref uuid) = album.cover {
                    cover_url = Some(format_cover_url(uuid, None));
                }
            }
        }
    }

    // Lyrics — prefer synced subtitles over plain text
    if let Some(ref l) = lyrics {
        tags.lyrics = l.subtitles.clone().or_else(|| l.lyrics.clone());
    }

    // Ensure album has a default
    if tags.album.is_none() {
        tags.album = Some("Unknown Album".to_string());
    }

    Ok(MetaTags { tags, cover_url })
}
