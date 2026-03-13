use anyhow::Result;

use super::client::{API_BASE, fetch_json, query_args};
use super::types::lyrics::Lyrics;
use super::types::openapi::{ApiTrack, ApiTracksResponse};
use super::types::playback::{BtsManifest, DecodedManifest, PlaybackInfo, PlaybackInfoResponse};
use super::types::quality::{AudioQuality, ManifestMimeType};
use super::types::track::Track;

use base64::Engine;

/// Fetch a single track by ID.
///
/// `GET /v1/tracks/{id}?countryCode=...&deviceType=DESKTOP&locale=...`
pub async fn track(track_id: &str) -> Result<Option<Track>> {
    let url = format!("{API_BASE}/tracks/{track_id}?{}", query_args());
    fetch_json(&url).await
}

/// Fetch playback info for a track at a given quality.
///
/// Returns the raw `PlaybackInfoResponse` with the manifest still
/// base64-encoded. Use [`playback_info`] for a decoded version.
///
/// `GET /v1/tracks/{id}/playbackinfo?audioquality=...&playbackmode=STREAM&assetpresentation=FULL`
pub async fn playback_info_raw(
    track_id: &str,
    audio_quality: AudioQuality,
) -> Result<Option<PlaybackInfoResponse>> {
    let url = format!(
        "{API_BASE}/tracks/{track_id}/playbackinfo?audioquality={}&playbackmode=STREAM&assetpresentation=FULL",
        audio_quality.as_str()
    );
    fetch_json(&url).await
}

/// Fetch playback info with the manifest decoded.
///
/// For BTS manifests the base64 payload is decoded into JSON
/// ([`BtsManifest`]). DASH manifests are left as raw XML text
/// wrapped in a [`DecodedManifest::Dash`] stub (DASH is unused in
/// the native player path — AAC tracks go through the browser).
pub async fn playback_info(
    track_id: &str,
    audio_quality: AudioQuality,
) -> Result<Option<PlaybackInfo>> {
    let raw = match playback_info_raw(track_id, audio_quality).await? {
        Some(r) => r,
        None => return Ok(None),
    };

    let manifest_bytes = base64::engine::general_purpose::STANDARD.decode(&raw.manifest)?;
    let manifest_text = String::from_utf8(manifest_bytes)?;

    let (decoded_manifest, mime_type) = match raw.manifest_mime_type {
        ManifestMimeType::Bts => {
            let bts: BtsManifest = serde_json::from_str(&manifest_text)?;
            let mime = bts.mime_type.clone();
            (DecodedManifest::Bts(bts), mime)
        }
        ManifestMimeType::Dash => {
            // DASH manifests are MPD XML — we store a minimal stub.
            // Full DASH parsing is not needed since the native player
            // only handles BTS/FLAC streams.
            let stub = super::types::playback::DashManifest {
                tracks: super::types::playback::DashTracks { audios: Vec::new() },
            };
            (DecodedManifest::Dash(stub), "audio/mp4".to_string())
        }
    };

    Ok(Some(PlaybackInfo {
        track_id: raw.track_id,
        asset_presentation: raw.asset_presentation,
        audio_mode: raw.audio_mode,
        audio_quality: raw.audio_quality,
        manifest_mime_type: raw.manifest_mime_type,
        album_replay_gain: raw.album_replay_gain,
        album_peak_amplitude: raw.album_peak_amplitude,
        track_replay_gain: raw.track_replay_gain,
        track_peak_amplitude: raw.track_peak_amplitude,
        bit_depth: raw.bit_depth,
        sample_rate: raw.sample_rate,
        mime_type: Some(mime_type),
        decoded_manifest,
    }))
}

/// Fetch lyrics for a track.
///
/// `GET /v1/tracks/{id}/lyrics?countryCode=...&deviceType=DESKTOP&locale=...`
pub async fn lyrics(track_id: &str) -> Result<Option<Lyrics>> {
    let url = format!("{API_BASE}/tracks/{track_id}/lyrics?{}", query_args());
    fetch_json(&url).await
}

/// Search tracks by ISRC via the OpenAPI v2 endpoint.
///
/// Returns all matching tracks across paginated results.
///
/// `GET /v2/tracks?countryCode=...&filter[isrc]=...`
pub async fn isrc(isrc: &str) -> Result<Vec<ApiTrack>> {
    let mut all_tracks = Vec::new();
    let initial_url = format!(
        "https://openapi.tidal.com/v2/tracks?{}&filter[isrc]={isrc}",
        query_args()
    );
    let mut next_url = Some(initial_url);

    while let Some(url) = next_url.take() {
        let resp: Option<ApiTracksResponse> = fetch_json(&url).await?;
        match resp {
            Some(r) if !r.data.is_empty() => {
                all_tracks.extend(r.data);
                next_url = r
                    .links
                    .and_then(|l| l.next)
                    .map(|path| format!("https://openapi.tidal.com/v2/{path}"));
            }
            _ => break,
        }
    }

    Ok(all_tracks)
}
