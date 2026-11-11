use crate::audio::flac_tags::Picture;
use crate::util::Refused;
use futures_util::StreamExt;
use std::time::Duration;

/// Bounds the whole accumulated buffer (all `MAX_URLS` segments share it), not one response.
/// Unbounded plugin-supplied urls would otherwise buffer fully before any check fires.
const MAX_DOWNLOAD_BYTES: usize = 512 * 1024 * 1024;
/// Cover art is an image, not an album.
const MAX_COVER_BYTES: usize = 16 * 1024 * 1024;

/// Bounds a hang, not throughput: a peer that connects and then sends nothing would leave
/// the renderer's promise pending for the life of the process.
const TRACK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Bounds the whole run, not one url. A 4096-url list each inside its own `TRACK_TIMEOUT` would
/// otherwise hold the single download permit for months.
const WHOLE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const COVER_TIMEOUT: Duration = Duration::from_secs(30);

/// Hosts TIDAL serves audio from. A label-boundary suffix, never a substring: `evil-audio.tidal.com`
/// is a host anyone can register and it contains the string.
fn is_tidal_media_host(host: &str) -> bool {
    host == "audio.tidal.com" || host.ends_with(".audio.tidal.com")
}

/// Cover art comes from its own host and does not belong on the media list.
fn is_tidal_artwork_host(host: &str) -> bool {
    host == "resources.tidal.com"
}

/// Host comes from the parsed url: `https://lgf.audio.tidal.com@evil.com/` reads as `evil.com` and is
/// refused; HTTPS only. General outbound http is the trust-gated native `http(s)` module. This must
/// not become a second, unprompted way to the same thing.
fn allowed_url(url: &str, allow: fn(&str) -> bool) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "https" && allow(parsed.host_str().unwrap_or(""))
}

/// The same rule at every redirect hop. Checking the submitted url alone would be bypassed by an
/// open redirect on an allowed host, and the shared client follows redirects with no policy at all.
/// Media and artwork share one client; either host is acceptable mid-chain.
fn redirect_allowed(url: &url::Url) -> bool {
    let host = url.host_str().unwrap_or("");
    url.scheme() == "https" && (is_tidal_media_host(host) || is_tidal_artwork_host(host))
}

/// Downloads use their own client. The redirect policy stays off every other caller of
/// `HTTP_CLIENT`.
static DOWNLOAD_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    crate::state::build_host_checked_client(
        redirect_allowed,
        "redirect left the allowed TIDAL hosts",
    )
});

/// What is left of the run's deadline, capped at the per-request limit; `None` once it has passed.
/// Derived from the deadline rather than tracked beside it: reqwest's timeout covers the streamed
/// body; clamping it is what bounds the run. A request admitted just before the deadline with its
/// own full `TRACK_TIMEOUT` would otherwise hold the single download permit half again as long.
fn next_request_timeout(remaining: Duration) -> Option<Duration> {
    (!remaining.is_zero()).then(|| TRACK_TIMEOUT.min(remaining))
}

/// Stream and stop at the ceiling: `Content-Length` is absent on a chunked response, and a
/// buffered body is already allocated by the time it could be measured.
pub(super) async fn fetch_track(urls: &[String]) -> anyhow::Result<Vec<u8>> {
    // `TRACK_TIMEOUT` bounds one url. With a list that long, a peer trickling a byte inside each
    // url's own window would hold the single download permit for months and block every other
    // plugin. The whole run gets a deadline too.
    let deadline = tokio::time::Instant::now() + WHOLE_DOWNLOAD_TIMEOUT;
    let mut out = Vec::new();
    for url in urls {
        if !allowed_url(url, is_tidal_media_host) {
            // Gated, like the ingress refusal in `super::mod`: the caller learns of it from its own
            // 403 (the log is not the only channel), and an ungated write would let a refused-call
            // loop drive the disk from the renderer.
            crate::vprintln!(
                "[DOWNLOAD] Refused a url outside the TIDAL media hosts: {}",
                crate::util::refused_host(url)
            );
            return Err(Refused("download url is not a TIDAL media url").into());
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(timeout) = next_request_timeout(remaining) else {
            anyhow::bail!("download exceeded {WHOLE_DOWNLOAD_TIMEOUT:?}");
        };
        let resp = DOWNLOAD_CLIENT.get(url).timeout(timeout).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("upstream status {}", resp.status());
        }
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if out.len() + chunk.len() > MAX_DOWNLOAD_BYTES {
                anyhow::bail!("download exceeded the ceiling of {MAX_DOWNLOAD_BYTES} bytes");
            }
            out.extend_from_slice(&chunk);
        }
    }
    Ok(out)
}

pub(super) async fn fetch_cover(url: &str) -> Option<Picture> {
    if !allowed_url(url, is_tidal_artwork_host) {
        crate::vprintln!("[DOWNLOAD] Cover url is not a TIDAL artwork url, skipping it");
        return None;
    }
    let resp = match DOWNLOAD_CLIENT.get(url).timeout(COVER_TIMEOUT).send().await {
        Ok(resp) => resp,
        Err(e) => {
            crate::vprintln!("[DOWNLOAD] Cover fetch failed: {e}");
            return None;
        }
    };
    if !resp.status().is_success() {
        crate::vprintln!("[DOWNLOAD] Cover fetch returned {}", resp.status());
        return None;
    }

    // The spec requires printable ASCII here; a parameterised or non-ASCII header
    // value is rejected rather than written into the block.
    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or(value).trim().to_string())
        .filter(|mime| mime.starts_with("image/") && mime.is_ascii())
        .unwrap_or_else(|| "image/jpeg".to_string());

    let mut data = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(e) => {
                crate::vprintln!("[DOWNLOAD] Cover body read failed: {e}");
                return None;
            }
        };
        if data.len() + chunk.len() > MAX_COVER_BYTES {
            crate::vprintln!("[DOWNLOAD] Cover art is over the size ceiling, skipping it");
            return None;
        }
        data.extend_from_slice(&chunk);
    }
    Some(Picture { mime, data })
}

#[cfg(test)]
#[path = "../../../../tests/unit/ipc/plugin/download/net.rs"]
mod tests;
