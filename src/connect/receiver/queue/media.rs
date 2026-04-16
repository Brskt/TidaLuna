//! Pure media resolution helpers.
//!
//! Functions in this module do not access `QueueManager` state. They either
//! transform a `QueueItem` into a `MediaInfo` (fully deterministic) or
//! resolve a `media_id` to a streaming URL by calling the TIDAL
//! `playbackinfo` endpoint (HTTP, but stateless aside from the `reqwest`
//! client and the content-server config passed in as arguments).
//!
//! Keeping these responsibilities outside the façade means the queue state
//! machine never needs to reason about DASH/BTS manifest parsing, and the
//! resolution logic is trivially unit-testable against synthesised inputs.

use base64::Engine as _;

use crate::connect::consts;
use crate::connect::types::{MediaInfo, MediaMetadata, QueueItem, ServerInfo};

use super::http::resolve_auth_header;

/// Build a `MediaInfo` payload from a cloud-queue item. Cloud-queue items
/// often carry richer data in `display_info` than in `metadata`, so we
/// fill the metadata from display_info whenever the primary field is
/// missing.
pub(super) fn queue_item_to_media_info(item: &QueueItem) -> MediaInfo {
    let mut metadata: Option<MediaMetadata> = item
        .metadata
        .as_ref()
        .and_then(|m| serde_json::from_value(m.clone()).ok());

    if let Some(ref di) = item.display_info {
        let md = metadata.get_or_insert(MediaMetadata {
            title: None,
            album_title: None,
            artists: None,
            duration: None,
            images: None,
        });
        if md.title.is_none() {
            md.title = di.title.clone();
        }
        if md.album_title.is_none() {
            md.album_title = di.album_title.clone();
        }
        if md.artists.is_none() {
            md.artists = di.artists.clone();
        }
        if md.images.is_none() {
            md.images = di.images.clone();
        }
        if md.duration.is_none() {
            md.duration = di.duration.or(item.duration_ms);
        }
    }

    MediaInfo {
        item_id: item.item_id.clone(),
        media_id: item.media_id.clone(),
        src_url: item.src_url.clone(),
        stream_type: None,
        metadata,
        custom_data: item.custom_data.clone(),
        media_type: 0,
        policy: crate::connect::types::default_policy(),
    }
}

/// Resolve a `mediaId` to a streaming URL via the TIDAL `playbackinfo`
/// endpoint. Fills `src_url`, `stream_type`, and (for encrypted streams)
/// `custom_data.encryptionKey` on the provided `media`.
///
/// Returns silently (`return;`) on any upstream failure: the caller treats
/// an unresolved `src_url` as "no playback" and continues the queue flow.
/// This matches the pre-extraction behaviour; turning these soft failures
/// into `Result`s is a separate concern left to a later phase.
pub(super) async fn resolve_media_url(
    http: &reqwest::Client,
    content_server: Option<&ServerInfo>,
    media: &mut MediaInfo,
) {
    if media.src_url.is_some() {
        return;
    }

    let cs = match content_server {
        Some(s) => s,
        None => {
            crate::vprintln!("[connect::queue] No content server for media resolution");
            return;
        }
    };

    let auth_header = resolve_auth_header(cs);
    let quality = cs
        .query_parameters
        .get("audioquality")
        .and_then(|v| v.as_str())
        .unwrap_or("HI_RES_LOSSLESS");

    let url = format!(
        "{}/tracks/{}/playbackinfo?audioquality={}&playbackmode=STREAM&assetpresentation=FULL",
        cs.server_url, media.media_id, quality
    );

    crate::vprintln!(
        "[connect::queue] Resolving media {} via {}",
        media.media_id,
        url
    );

    let resp = match http
        .get(&url)
        .header("Authorization", &auth_header)
        .timeout(std::time::Duration::from_secs(consts::HTTP_TIMEOUT_SECS))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            crate::vprintln!("[connect::queue] playbackinfo HTTP {}", r.status());
            return;
        }
        Err(e) => {
            crate::vprintln!("[connect::queue] playbackinfo request failed: {}", e);
            return;
        }
    };

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            crate::vprintln!("[connect::queue] playbackinfo parse failed: {}", e);
            return;
        }
    };

    let manifest_b64 = match body.get("manifest").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => {
            crate::vprintln!("[connect::queue] playbackinfo: no manifest field");
            return;
        }
    };

    let manifest_mime = body
        .get("manifestMimeType")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let manifest_bytes = match base64::engine::general_purpose::STANDARD.decode(manifest_b64) {
        Ok(b) => b,
        Err(e) => {
            crate::vprintln!("[connect::queue] manifest base64 decode failed: {}", e);
            return;
        }
    };

    if manifest_mime == "application/dash+xml" {
        apply_dash_manifest(&manifest_bytes, media);
        return;
    }

    // BTS/JSON manifest (FLAC, lossless)
    apply_bts_manifest(&manifest_bytes, media);
}

/// Parse a DASH MPD manifest and inject the resulting URL / codec / segment
/// list / duration into `media`.
fn apply_dash_manifest(manifest_bytes: &[u8], media: &mut MediaInfo) {
    let manifest_str = String::from_utf8_lossy(manifest_bytes).to_string();
    crate::vprintln!(
        "[connect::queue] DASH manifest ({} bytes)",
        manifest_str.len()
    );
    match crate::player::dash::parse_dash_mpd(&manifest_str) {
        Ok(dash) => {
            media.src_url = Some(dash.init_url);
            media.stream_type = Some(
                dash.codec
                    .split('.')
                    .next()
                    .unwrap_or(&dash.codec)
                    .to_string(),
            );
            let cd = media
                .custom_data
                .get_or_insert_with(|| serde_json::json!({}));
            if !cd.is_object() {
                *cd = serde_json::json!({});
            }
            cd["dashSegmentUrls"] = serde_json::to_value(&dash.segment_urls).unwrap_or_default();
            if let Some(secs) = dash.duration_secs
                && let Some(ref mut md) = media.metadata
                && md.duration.is_none()
            {
                md.duration = Some((secs * 1000.0) as u64);
            }
            crate::vprintln!(
                "[connect::queue] DASH resolved: {} segments",
                dash.segment_urls.len()
            );
        }
        Err(e) => {
            crate::vprintln!("[connect::queue] DASH parse failed: {}", e);
        }
    }
}

/// Parse a BTS JSON manifest (FLAC/lossless) and inject the resulting URL /
/// codec / encryption key into `media`.
fn apply_bts_manifest(manifest_bytes: &[u8], media: &mut MediaInfo) {
    let manifest: serde_json::Value = match serde_json::from_slice(manifest_bytes) {
        Ok(v) => v,
        Err(e) => {
            crate::vprintln!("[connect::queue] manifest JSON parse failed: {}", e);
            return;
        }
    };

    if let Some(urls) = manifest.get("urls").and_then(|v| v.as_array())
        && let Some(stream_url) = urls.first().and_then(|v| v.as_str())
    {
        media.src_url = Some(stream_url.to_string());
        crate::vprintln!("[connect::queue] Resolved: {}", stream_url);
    }
    if let Some(codecs) = manifest.get("codecs").and_then(|v| v.as_str()) {
        let base_codec = codecs.split('.').next().unwrap_or(codecs);
        media.stream_type = Some(base_codec.to_string());
    }
    if let Some(key_id) = manifest.get("keyId").and_then(|v| v.as_str())
        && !key_id.is_empty()
    {
        let cd = media
            .custom_data
            .get_or_insert_with(|| serde_json::json!({}));
        if !cd.is_object() {
            *cd = serde_json::json!({});
        }
        cd["encryptionKey"] = serde_json::Value::String(key_id.to_string());
    }
}
