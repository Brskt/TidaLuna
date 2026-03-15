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
/// ([`BtsManifest`]). DASH manifests are parsed from MPD XML using
/// `dash-mpd` and segment URLs are extracted from the SegmentTemplate.
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

    crate::vprintln!(
        "[DBG:playback_info] track={} quality={:?} mime_type={:?}",
        track_id,
        raw.audio_quality,
        raw.manifest_mime_type
    );

    let (decoded_manifest, mime_type) = match raw.manifest_mime_type {
        ManifestMimeType::Bts => {
            let bts: BtsManifest = serde_json::from_str(&manifest_text)?;
            crate::vprintln!(
                "[DBG:playback_info] BTS manifest: mime={} codecs={} enc={:?} urls={} key={:?}",
                bts.mime_type,
                bts.codecs,
                bts.encryption_type,
                bts.urls.len(),
                bts.key_id.as_deref().map(|k| &k[..k.len().min(8)])
            );
            let mime = bts.mime_type.clone();
            (DecodedManifest::Bts(bts), mime)
        }
        ManifestMimeType::Dash => {
            let dash = parse_dash_mpd(&manifest_text)?;
            crate::vprintln!(
                "[DBG:playback_info] DASH parsed: codec={} sampleRate={:?} bandwidth={:?} segments={}",
                dash.codec,
                dash.sample_rate,
                dash.bandwidth,
                dash.segment_urls.len()
            );
            (DecodedManifest::Dash(dash), "audio/mp4".to_string())
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

/// Parse a DASH MPD XML string and extract segment URLs.
fn parse_dash_mpd(xml: &str) -> Result<super::types::playback::DashManifest> {
    // TIDAL uses group="main" (string) which violates the DASH spec (expects integer).
    // Remove non-standard attributes before parsing.
    let cleaned = xml.replace(r#" group="main""#, "");
    let mpd = dash_mpd::parse(&cleaned).map_err(|e| anyhow::anyhow!("Failed to parse MPD: {e}"))?;

    let period = mpd
        .periods
        .first()
        .ok_or_else(|| anyhow::anyhow!("MPD has no periods"))?;

    let adaptation = period
        .adaptations
        .first()
        .ok_or_else(|| anyhow::anyhow!("MPD period has no adaptation sets"))?;

    let repr = adaptation
        .representations
        .first()
        .ok_or_else(|| anyhow::anyhow!("MPD adaptation set has no representations"))?;

    let codec = repr.codecs.clone().unwrap_or_default();
    let sample_rate = repr
        .audioSamplingRate
        .as_deref()
        .and_then(|s| s.parse::<u32>().ok());
    let bandwidth = repr.bandwidth.map(|b| b as u32);

    let seg_tpl = repr
        .SegmentTemplate
        .as_ref()
        .or(adaptation.SegmentTemplate.as_ref())
        .ok_or_else(|| anyhow::anyhow!("No SegmentTemplate found in MPD"))?;

    let init_url = seg_tpl
        .initialization
        .clone()
        .ok_or_else(|| anyhow::anyhow!("SegmentTemplate has no initialization URL"))?;

    let media_tpl = seg_tpl
        .media
        .clone()
        .ok_or_else(|| anyhow::anyhow!("SegmentTemplate has no media URL template"))?;

    let start_number = seg_tpl.startNumber.unwrap_or(1);

    let mut segment_urls = Vec::new();
    if let Some(timeline) = &seg_tpl.SegmentTimeline {
        let mut number = start_number;
        for s in &timeline.segments {
            let repeat = s.r.unwrap_or(0).max(0) as u64 + 1;
            for _ in 0..repeat {
                segment_urls.push(media_tpl.replace("$Number$", &number.to_string()));
                number += 1;
            }
        }
    } else if let Some(duration) = seg_tpl.duration {
        let timescale = seg_tpl.timescale.unwrap_or(1) as f64;
        if let Some(mpd_dur) = mpd.mediaPresentationDuration.as_ref() {
            let total_secs = mpd_dur.as_secs_f64();
            let seg_dur_secs = duration / timescale;
            let count = (total_secs / seg_dur_secs).ceil() as u64;
            for i in 0..count {
                segment_urls.push(media_tpl.replace("$Number$", &(start_number + i).to_string()));
            }
        }
    }

    crate::vprintln!("[DASH]   init_url={}", &init_url[..init_url.len().min(80)]);
    crate::vprintln!(
        "[DASH]   {} segment URLs, codec={}, sampleRate={:?}",
        segment_urls.len(),
        codec,
        sample_rate
    );

    Ok(super::types::playback::DashManifest {
        init_url,
        segment_urls,
        codec,
        sample_rate,
        bandwidth,
    })
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
