use super::cache_entry::{CacheReadError, read_cache_entry};
use super::{LoadContext, LoadStep};
use crate::audio::{download, preload};
use crate::player::buffer::{DownloadOwner, HeadStatus, RamBuffer};
use crate::player::{MediaErrorCode, cache, format_bytes, format_ms, short_id};
use crate::state::{AUDIO_CACHE, HTTP_CLIENT_PLAYBACK, TrackInfo};

fn http_version_str(v: reqwest::Version) -> &'static str {
    if v == reqwest::Version::HTTP_3 {
        "3"
    } else if v == reqwest::Version::HTTP_2 {
        "2"
    } else if v == reqwest::Version::HTTP_11 {
        "1.1"
    } else if v == reqwest::Version::HTTP_10 {
        "1.0"
    } else {
        "?"
    }
}

/// Extract the CDN cache status from the response headers (x-cache).
pub(crate) fn cdn_cache_status(resp: &reqwest::Response) -> &str {
    resp.headers()
        .get("x-cache")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("?")
}

pub(crate) fn log_response_headers(resp: &reqwest::Response, prefix: &str) {
    use std::fmt::Write as _;
    let h = resp.headers();
    let ver = http_version_str(resp.version());
    let ct = h
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    let mut info = format!("HTTP/{ver} | type={ct}");
    for &(header, label) in &[
        ("content-encoding", "enc"),
        ("server", "server"),
        ("x-cache", "CDN"),
        ("x-cache-hits", "hits"),
        ("age", "age"),
        ("via", "via"),
    ] {
        if let Some(val) = h.get(header).and_then(|v| v.to_str().ok()) {
            let _ = write!(info, " | {label}={val}");
        }
    }
    crate::vprintln!("{prefix} {info}");
}

/// `key` is the track's wrapped content key: cache files hold TIDAL's
/// ciphertext; a hit is only playable once decrypted. An empty key means the
/// stream was never encrypted, matching the download path's own check.
pub(super) async fn try_cache_hit(ctx: &LoadContext, track_id: &str, key: &str) -> LoadStep {
    let cache_t0 = std::time::Instant::now();

    // Short lock: index lookup only (no disk I/O)
    let path = {
        let Ok(cache) = AUDIO_CACHE.lock() else {
            crate::vprintln!("[CACHE]  Lock poisoned, skipping cache lookup");
            return LoadStep::Miss;
        };
        cache.lookup_seen(track_id)
    };

    let Some(cache::Seen { path, generation }) = path else {
        let cache_ms = cache_t0.elapsed().as_secs_f64() * 1000.0;
        crate::vprintln!(
            "[CACHE]  Miss ({}) | lookup: {}",
            short_id(track_id, 40),
            format_ms(cache_ms)
        );
        return LoadStep::Miss;
    };

    // Gate before the read, not after: skipping through cached tracks would otherwise
    // decrypt each one in full only to throw the result away.
    if ctx.is_stale() {
        crate::vprintln!("[LOAD #{}] stale before cache read, dropping", ctx.load_gen);
        return LoadStep::Handled;
    }

    let read = {
        let path = path.clone();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || read_cache_entry(&path, &key)).await
    };

    let data = match read {
        Ok(Ok(data)) => data,
        Ok(Err(CacheReadError::Orphaned)) => {
            if let Ok(mut cache) = AUDIO_CACHE.lock() {
                cache.remove_orphan(track_id, generation);
            }
            return LoadStep::Miss;
        }
        Ok(Err(CacheReadError::Unreadable(reason))) => {
            crate::vprintln!(
                "[CACHE]  Keeping entry {}: {reason}",
                short_id(track_id, 40)
            );
            return LoadStep::Miss;
        }
        Ok(Err(CacheReadError::Corrupt(reason))) => {
            if let Ok(mut cache) = AUDIO_CACHE.lock() {
                match cache.drop_seen(track_id, generation) {
                    cache::DropOutcome::Dropped | cache::DropOutcome::NoRow => crate::vprintln!(
                        "[CACHE]  Dropped unusable entry {}: {reason}",
                        short_id(track_id, 40)
                    ),
                    // The row outlives the failed delete, leaving `lookup_path` to hand back
                    // the same bytes on every later read. Ungated because the only retries (an
                    // eviction pass, the next store of this id) are not guaranteed to come.
                    cache::DropOutcome::FileKept => crate::verr!(
                        "[CACHE]  Unusable entry {} will keep failing, its file would not go: {reason}",
                        short_id(track_id, 40)
                    ),
                }
            }
            return LoadStep::Miss;
        }
        Err(e) => {
            crate::vprintln!("[CACHE]  Read task failed: {e}");
            return LoadStep::Miss;
        }
    };

    // Short lock: update access metadata
    if let Ok(cache) = AUDIO_CACHE.lock() {
        cache.touch(track_id);
    }

    let cache_ms = cache_t0.elapsed().as_secs_f64() * 1000.0;
    if ctx.is_stale() {
        return LoadStep::Handled;
    }
    crate::vprintln!(
        "[CACHE]  Hit: {} | {} | read: {} | total: {}",
        short_id(track_id, 40),
        format_bytes(data.len() as u64),
        format_ms(cache_ms),
        format_ms(ctx.load_start.elapsed().as_secs_f64() * 1000.0)
    );
    let buffer = RamBuffer::from_complete(data);
    ctx.publish_load(buffer, Some(generation), track_id.to_string());
    LoadStep::Handled
}

pub(super) async fn try_preload_hit(
    ctx: &LoadContext,
    track: &TrackInfo,
    track_id: &str,
) -> LoadStep {
    let preload_t0 = std::time::Instant::now();
    if let Some(preloaded) = preload::take_preloaded_if_match(track).await {
        let preload_ms = preload_t0.elapsed().as_secs_f64() * 1000.0;
        if ctx.is_stale() {
            crate::vprintln!(
                "[LOAD #{}] stale after preload check, dropping",
                ctx.load_gen
            );
            // The take spent the record and adopted this download onto the playback queue;
            // nothing else names it any more. Dropped without this it runs to the end of the
            // file, competing for bandwidth with the track the listener actually skipped to.
            preloaded.buffer.cancel_download();
            return LoadStep::Handled;
        }
        // Adopted is not the same as usable: the slot is filled the instant the download
        // starts, and these bytes may still be in flight. The player thread probes this buffer
        // with a blocking read: the wait belongs here, in this task, and not there.
        if !preload::head_has_landed(&preloaded.buffer, || !ctx.is_stale()).await {
            // Spent and unusable: the record is gone, and nothing else will stop this
            // download. A stale load wanted nothing anyway; otherwise the ordinary path
            // below opens the copy that will actually be listened to.
            preloaded.buffer.cancel_download();
            if ctx.is_stale() {
                return LoadStep::Handled;
            }
            crate::vprintln!("[PRELOAD] Adopted copy never became playable, loading it afresh");
            return LoadStep::Miss;
        }
        crate::vprintln!(
            "[PRELOAD] Hit: {} | check: {} | total: {}",
            format_bytes(preloaded.buffer.total_len()),
            format_ms(preload_ms),
            format_ms(ctx.load_start.elapsed().as_secs_f64() * 1000.0)
        );
        let buffer = preloaded.buffer;
        ctx.publish_load(buffer, None, track_id.to_string());
        return LoadStep::Handled;
    }

    let preload_ms = preload_t0.elapsed().as_secs_f64() * 1000.0;
    crate::vprintln!("[PRELOAD] Miss | check: {}", format_ms(preload_ms));
    LoadStep::Miss
}

pub(super) async fn start_stream_load(ctx: &LoadContext, url: &str, key: &str, track_id: &str) {
    let load_gen = ctx.load_gen;

    if ctx.is_stale() {
        crate::vprintln!("[LOAD #{load_gen}] stale before HTTP, dropping");
        return;
    }

    let req_start = std::time::Instant::now();
    let resp = match HTTP_CLIENT_PLAYBACK.get(url).send().await {
        Ok(r) => {
            if !r.status().is_success() {
                let status = r.status();
                crate::vprintln!("[ERROR]  Upstream status: {}", status);
                ctx.fail_load(format!("HTTP {}", status), MediaErrorCode::NoSuchFile);
                return;
            }
            r
        }
        Err(e) => {
            let text = crate::util::network_error_text(e);
            crate::vprintln!("[ERROR]  Request failed: {text}");
            ctx.fail_load(
                format!("request failed: {text}"),
                MediaErrorCode::NoSuchFile,
            );
            return;
        }
    };
    let ttfb_ms = req_start.elapsed().as_secs_f64() * 1000.0;
    let cdn = cdn_cache_status(&resp);
    crate::vprintln!(
        "[NET]    TTFB: {} | CDN: {} | HTTP/{}",
        format_ms(ttfb_ms),
        cdn,
        http_version_str(resp.version())
    );
    log_response_headers(&resp, "[NET]   ");

    if ctx.is_stale() {
        crate::vprintln!("[LOAD #{load_gen}] stale after HTTP TTFB, dropping");
        return;
    }

    let total_len = resp.content_length().unwrap_or(0);
    if total_len == 0 {
        crate::vprintln!(
            "[FETCH]  No Content-Length, full download... (TTFB: {})",
            format_ms(ttfb_ms)
        );
        // Handed the response already open, not left to race a second one against it: a
        // body nobody reads keeps an HTTP/1.1 connection held and unusable until it drops,
        // and this one would not drop until the whole fallback download had finished.
        match download::fetch_and_decrypt(url, key, resp, &ctx.governor).await {
            Ok(fetched) => {
                if ctx.is_stale() {
                    crate::vprintln!("[LOAD #{load_gen}] stale after full download, dropping");
                    return;
                }
                crate::vprintln!(
                    "[FETCH]  Done ({} in {:.0}ms)",
                    format_bytes(fetched.data.len() as u64),
                    ctx.load_start.elapsed().as_secs_f64() * 1000.0
                );
                let buffer =
                    RamBuffer::from_complete_with_ciphertext(fetched.data, fetched.ciphertext);
                ctx.publish_load(buffer, None, track_id.to_string());
            }
            Err(e) => {
                crate::vprintln!("[ERROR]  Fetch failed: {}", e);
                ctx.fail_load(format!("fetch failed: {e}"), MediaErrorCode::NoSuchFile);
            }
        }
        return;
    }

    crate::vprintln!(
        "[NET]    Size: {} (load #{load_gen})",
        format_bytes(total_len)
    );

    // The load's own token becomes the buffer's, rather than a second one beside it: the
    // next load reaches this download through the player's slot, a buffer swap reaches it
    // through the buffer, and both arrive at the same token.
    let (buffer, writer) = RamBuffer::new(
        total_len,
        DownloadOwner::Playback,
        ctx.cancel_token.clone(),
        ctx.governor.clone(),
    );

    download::start_download(resp, url.to_string(), key.to_string(), writer);

    // Pre-buffer 64KB before handing to decoder
    const PRE_BUFFER_TARGET: u64 = 64 * 1024;
    const PRE_BUFFER_TIMEOUT_MS: u64 = 2000;

    let prebuf_start = std::time::Instant::now();
    let prebuf_deadline = prebuf_start + std::time::Duration::from_millis(PRE_BUFFER_TIMEOUT_MS);
    loop {
        if ctx.is_stale() {
            crate::vprintln!("[LOAD #{load_gen}] stale during pre-buffer, dropping");
            buffer.cancel();
            return;
        }
        match buffer.head_status(PRE_BUFFER_TARGET) {
            HeadStatus::Landed => break,
            // Published all the same, and that is deliberate: the buffer carries the failure,
            // `RamBuffer::read` hands it to the decode thread on the first read before any
            // range check, and the listener gets a media error out of it. Refusing to publish
            // would swallow the only report of the failure there is. Only the waiting was
            // ever wrong here: only the waiting ends.
            HeadStatus::Ended(_) => {
                crate::vprintln!(
                    "[LOAD #{load_gen}] download ended at {}KB/{}KB, publishing what landed",
                    buffer.written() / 1024,
                    PRE_BUFFER_TARGET / 1024
                );
                break;
            }
            HeadStatus::Filling => {}
        }
        let remaining = prebuf_deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            crate::vprintln!(
                "[LOAD #{load_gen}] pre-buffer timeout ({}KB/{}KB in {}ms)",
                buffer.written() / 1024,
                PRE_BUFFER_TARGET / 1024,
                PRE_BUFFER_TIMEOUT_MS
            );
            break;
        }
        let _ = tokio::time::timeout(remaining, buffer.notified()).await;
    }

    crate::vprintln!(
        "[LOAD #{load_gen}] pre-buffered {}KB in {:.0}ms, sending Load",
        buffer.written() / 1024,
        prebuf_start.elapsed().as_secs_f64() * 1000.0
    );
    ctx.publish_load(buffer, None, track_id.to_string());
}
