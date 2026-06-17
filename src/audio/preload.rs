use crate::audio::bandwidth::TrafficClass;
use crate::audio::decrypt::FlacDecryptor;
use crate::player::buffer::RamBufferWriter;
use crate::state::{GOVERNOR, HTTP_CLIENT, PRELOAD_STATE, PreloadedTrack, TrackInfo};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

const PRELOAD_MAX_BYTES: usize = 32 * 1024 * 1024; // 32 MB

use crate::util::fmt::{format_bytes, format_ms};

async fn fetch_and_decrypt_inner(
    url: &str,
    key: &str,
    max_bytes: Option<usize>,
) -> anyhow::Result<Option<Vec<u8>>> {
    let start = std::time::Instant::now();
    let resp = HTTP_CLIENT.get(url).send().await?;

    if !resp.status().is_success() {
        anyhow::bail!("Upstream status: {}", resp.status());
    }

    let decryptor = if key.is_empty() {
        None
    } else {
        Some(FlacDecryptor::new(key)?)
    };
    let mut stream = resp.bytes_stream();
    let mut offset = 0u64;
    let mut decrypt_buf = Vec::new();
    let mut buffer = Vec::new();
    let mut reconnect_attempts: u32 = 0;
    const MAX_PRELOAD_RECONNECTS: u32 = 8;

    'download: loop {
        let item = match stream.next().await {
            Some(item) => item,
            None => break 'download,
        };
        let chunk = match item {
            Ok(chunk) => chunk,
            Err(e) => {
                // Idle connection died on a long pause: reconnect at `offset` and keep
                // appending - the decrypt offset stays aligned so resumed bytes decrypt
                // correctly. A 416 means `offset` is at/past EOF (all bytes buffered): finish.
                crate::vprintln!("[PRELOAD] Stream error at byte {offset}: {e}");
                stream = 'reconnect: loop {
                    reconnect_attempts += 1;
                    if reconnect_attempts > MAX_PRELOAD_RECONNECTS {
                        anyhow::bail!(
                            "network error after {MAX_PRELOAD_RECONNECTS} reconnects: {e}"
                        );
                    }
                    let backoff = std::time::Duration::from_millis(250 * reconnect_attempts as u64);
                    tokio::time::sleep(backoff).await;
                    let range_header = format!("bytes={offset}-");
                    match HTTP_CLIENT
                        .get(url)
                        .header("Range", &range_header)
                        .send()
                        .await
                    {
                        Ok(r) if r.status() == reqwest::StatusCode::PARTIAL_CONTENT => {
                            crate::vprintln!(
                                "[PRELOAD] Reconnected at byte {offset} (attempt {reconnect_attempts})"
                            );
                            break 'reconnect r.bytes_stream();
                        }
                        Ok(r) if r.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE => {
                            break 'download;
                        }
                        Ok(r) => anyhow::bail!("reconnect status: {}", r.status()),
                        Err(send_err) => {
                            crate::vprintln!(
                                "[PRELOAD] reconnect attempt {reconnect_attempts} failed: {send_err}"
                            );
                            continue 'reconnect;
                        }
                    }
                };
                continue 'download; // new stream, buffer + offset intact
            }
        };

        reconnect_attempts = 0; // received data; reset the retry budget

        GOVERNOR
            .acquire(TrafficClass::Preload, chunk.len() as u32)
            .await;

        decrypt_buf.clear();
        decrypt_buf.extend_from_slice(&chunk);
        if let Some(ref dec) = decryptor {
            dec.decrypt_in_place(&mut decrypt_buf, offset)?;
        }
        offset += chunk.len() as u64;

        if let Some(limit) = max_bytes
            && buffer.len().saturating_add(decrypt_buf.len()) > limit
        {
            let elapsed = start.elapsed().as_secs_f64();
            crate::vprintln!(
                "[PRELOAD] Skip RAM cache: size > {} (received {} in {:.1}s)",
                format_bytes(limit as u64),
                format_bytes(offset),
                elapsed
            );
            return Ok(None);
        }

        buffer.extend_from_slice(&decrypt_buf);
    }

    let elapsed = start.elapsed().as_secs_f64();
    let rate_mbps = (offset as f64 * 8.0) / (elapsed * 1_000_000.0);
    crate::vprintln!(
        "[FETCH]  {:.1} MB in {:.1}s ({:.1} Mbps)",
        offset as f64 / 1_048_576.0,
        elapsed,
        rate_mbps
    );

    Ok(Some(buffer))
}

pub async fn fetch_and_decrypt(url: &str, key: &str) -> anyhow::Result<Vec<u8>> {
    match fetch_and_decrypt_inner(url, key, None).await? {
        Some(buffer) => Ok(buffer),
        None => anyhow::bail!("unexpected capped fetch in uncapped mode"),
    }
}

pub async fn start_preload(track: TrackInfo) {
    cancel_preload().await;

    {
        let mut lock = PRELOAD_STATE.lock().await;
        lock.next_track = Some(track.clone());
    }

    let handle = tokio::spawn(async move {
        if track.url.is_empty() {
            return;
        }

        crate::vprintln!("[PRELOAD] Starting preload for next track");
        match fetch_and_decrypt_inner(&track.url, &track.key, Some(PRELOAD_MAX_BYTES)).await {
            Ok(Some(data)) => {
                if !data.is_empty() {
                    let mut lock = PRELOAD_STATE.lock().await;
                    if lock.next_track.as_ref() == Some(&track) {
                        lock.data = Some(PreloadedTrack { track, data });
                    }
                }
            }
            Ok(None) => {
                // Too large for RAM cache; keep only next_track so auto-load can still proceed.
            }
            Err(e) => {
                crate::vprintln!("[PRELOAD] Failed: {}", e);
            }
        }
    });

    let mut lock = PRELOAD_STATE.lock().await;
    lock.task = Some(handle);
}

pub async fn cancel_preload() {
    let mut lock = PRELOAD_STATE.lock().await;
    if let Some(handle) = lock.task.take() {
        handle.abort();
    }
    lock.data = None;
    lock.next_track = None;
}

/// Return the next track info (set during preload) without consuming preloaded data.
/// Used by the auto-load logic after "completed" to know which track to load next.
pub async fn take_next_track() -> Option<TrackInfo> {
    let mut lock = PRELOAD_STATE.lock().await;
    lock.next_track.take()
}

pub async fn take_preloaded_if_match(track: &TrackInfo) -> Option<PreloadedTrack> {
    let mut lock = PRELOAD_STATE.lock().await;
    if let Some(data) = lock.data.as_ref()
        && data.track == *track
    {
        lock.next_track = None;
        return lock.data.take();
    }
    None
}

/// Start a streaming download into a RamBufferWriter.
/// Handles decryption, governor rate limiting, and Range restarts.
pub fn start_download(
    resp: reqwest::Response,
    url: String,
    key: String,
    writer: RamBufferWriter,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    // On cancel, dropping the inner future drops the stream + writer, which closes
    // the HTTP connection and frees the buffer at once.
    tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {}
            _ = download_stream(resp, url, key, writer) => {}
        }
    })
}

async fn download_stream(
    resp: reqwest::Response,
    url: String,
    key: String,
    writer: RamBufferWriter,
) {
    let download_start = std::time::Instant::now();
    let decryptor = if key.is_empty() {
        None
    } else {
        match FlacDecryptor::new(&key) {
            Ok(d) => Some(d),
            Err(e) => {
                writer.finish_with_error(format!("decrypt init failed: {e}"));
                return;
            }
        }
    };

    let mut current_resp = Some(resp);
    let mut range_restarts: u32 = 0;
    let mut http_requests: u32 = 1; // initial GET counts as 1
    // Consecutive reconnect attempts after a mid-stream transport error,
    // reset on the next successful chunk. Bounds a truly-dead network.
    let mut reconnect_attempts: u32 = 0;
    const MAX_RECONNECTS: u32 = 8;

    'outer: loop {
        let (stream_resp, stream_offset) = if let Some(r) = current_resp.take() {
            (r, 0u64)
        } else {
            // Range restart requested
            let restart_t0 = std::time::Instant::now();
            let target = match writer.take_restart_target() {
                Some(t) => t,
                None => break,
            };
            // A seek restart starts a fresh stream, so clear the reconnect
            // budget - a `continue 'outer` from the reconnect loop lands here.
            reconnect_attempts = 0;

            // Brief debounce to coalesce rapid seeks
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            let target = writer.take_restart_target().unwrap_or(target);

            if writer.is_cancelled() {
                return;
            }

            let range_header = format!("bytes={target}-");
            crate::vprintln2!("[STREAM] Range restart at byte {target}");
            crate::vprintln3!(
                "[STREAM] Range restart #{} at byte {target} | http_reqs={} elapsed={}",
                range_restarts + 1,
                http_requests,
                format_ms(download_start.elapsed().as_secs_f64() * 1000.0),
            );

            let send_fut = crate::state::HTTP_CLIENT_PLAYBACK
                .get(&url)
                .header("Range", &range_header)
                .send();
            let range_resp = tokio::select! {
                biased;
                _ = writer.wait_for_restart_or_cancel() => {
                    if writer.is_cancelled() {
                        return;
                    }
                    crate::vprintln3!("[STREAM] Restart aborted (new restart pending) after {}", format_ms(restart_t0.elapsed().as_secs_f64() * 1000.0));
                    continue 'outer;
                }
                result = send_fut => {
                    match result {
                        Ok(r) => r,
                        Err(e) => {
                            crate::vprintln3!(
                                "[STREAM] DOWNLOAD DIED: range request failed at byte {target} | restarts={range_restarts}: {e}"
                            );
                            writer.finish_with_error(format!("range request failed: {e}"));
                            return;
                        }
                    }
                }
            };

            let cdn = crate::player::cdn_cache_status(&range_resp);
            let ttfb = format_ms(restart_t0.elapsed().as_secs_f64() * 1000.0);
            crate::vprintln2!(
                "[STREAM] TTFB: {} | CDN: {} | Range: bytes={}-",
                ttfb,
                cdn,
                target
            );
            crate::player::log_response_headers(&range_resp, "[NET]   ");

            let status = range_resp.status();
            if status == reqwest::StatusCode::PARTIAL_CONTENT {
                writer.reset_for_range(target);
                range_restarts += 1;
                http_requests += 1;
                (range_resp, target)
            } else if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
                writer.finish();
                break;
            } else if status.is_success() {
                // Server ignored Range, restart from beginning
                crate::vprintln2!("[STREAM] Server ignored Range header, restarting from byte 0");
                writer.reset_for_range(0);
                (range_resp, 0u64)
            } else {
                crate::vprintln3!(
                    "[STREAM] DOWNLOAD DIED: range status {status} at byte {target} | restarts={range_restarts}"
                );
                writer.finish_with_error(format!("range request status: {status}"));
                return;
            }
        };

        let mut stream = stream_resp.bytes_stream();
        let mut offset = stream_offset;
        let mut decrypt_buf = Vec::with_capacity(128 * 1024);
        let mut chunk_count: u32 = 0;
        let mut bytes_since_restart: u64 = 0;
        let stream_start = std::time::Instant::now();
        let mut last_progress_bytes: u64 = 0;

        loop {
            let chunk_opt = tokio::select! {
                biased;
                _ = writer.wait_for_restart_or_cancel() => {
                    if writer.is_cancelled() {
                        return;
                    }
                    crate::vprintln3!(
                        "[STREAM] Interrupted: new restart after {}KB in {} ({} chunks)",
                        bytes_since_restart / 1024,
                        format_ms(stream_start.elapsed().as_secs_f64() * 1000.0),
                        chunk_count
                    );
                    continue 'outer;
                }
                result = stream.next() => result,
            };

            match chunk_opt {
                Some(Ok(chunk)) => {
                    // Select between governor throttle and restart/cancel.
                    // When the governor throttles playback (buffer full),
                    // we must still respond to seek restarts.
                    tokio::select! {
                        biased;
                        _ = writer.wait_for_restart_or_cancel() => {
                            if writer.is_cancelled() {
                                return;
                            }
                            continue 'outer;
                        }
                        _ = GOVERNOR.acquire(TrafficClass::Playback, chunk.len() as u32) => {}
                    }

                    decrypt_buf.clear();
                    decrypt_buf.extend_from_slice(&chunk);
                    match decryptor
                        .as_ref()
                        .map(|d| d.decrypt_in_place(&mut decrypt_buf, offset))
                        .unwrap_or(Ok(()))
                    {
                        Ok(()) => {
                            offset += chunk.len() as u64;
                            reconnect_attempts = 0; // progress made; reset the reconnect budget
                            let written = writer.write_counted(&decrypt_buf);
                            chunk_count += 1;
                            bytes_since_restart += chunk.len() as u64;

                            // Log first chunk + progress every 512KB
                            if chunk_count == 1 {
                                crate::vprintln3!(
                                    "[STREAM] First chunk: {}B at {}",
                                    chunk.len(),
                                    format_ms(stream_start.elapsed().as_secs_f64() * 1000.0),
                                );
                            }
                            if !written {
                                crate::vprintln3!(
                                    "[STREAM] Write DISCARDED (restart pending) at {}KB",
                                    bytes_since_restart / 1024
                                );
                            }
                            if bytes_since_restart - last_progress_bytes >= 512 * 1024 {
                                crate::vprintln3!(
                                    "[STREAM] Progress: {}KB in {} ({} chunks)",
                                    bytes_since_restart / 1024,
                                    format_ms(stream_start.elapsed().as_secs_f64() * 1000.0),
                                    chunk_count,
                                );
                                last_progress_bytes = bytes_since_restart;
                            }
                        }
                        Err(e) => {
                            writer.finish_with_error(format!("decrypt error: {e}"));
                            return;
                        }
                    }
                }
                Some(Err(e)) => {
                    // Transient transport error (idle connection died on a long
                    // pause, or a blip): reconnect at `offset` and keep appending -
                    // [base..offset] stays valid so the decode blocks at the frontier
                    // rather than ending the track. Terminal/exhausted retries end it.
                    if writer.is_cancelled() {
                        return;
                    }
                    crate::vprintln!("[STREAM] Stream error at byte {offset}: {e}");
                    stream = 'reconnect: loop {
                        if writer.has_restart_pending() {
                            continue 'outer; // a seek supersedes the reconnect
                        }
                        reconnect_attempts += 1;
                        if reconnect_attempts > MAX_RECONNECTS {
                            crate::vprintln3!(
                                "[STREAM] DOWNLOAD DIED: {MAX_RECONNECTS} reconnects exhausted at byte {offset} | restarts={range_restarts}"
                            );
                            writer.finish_with_error(format!(
                                "network error after {MAX_RECONNECTS} reconnects: {e}"
                            ));
                            return;
                        }
                        let backoff =
                            std::time::Duration::from_millis(250 * reconnect_attempts as u64);
                        tokio::select! {
                            biased;
                            _ = writer.wait_for_restart_or_cancel() => {
                                if writer.is_cancelled() {
                                    return;
                                }
                                continue 'outer; // seek/cancel during backoff
                            }
                            _ = tokio::time::sleep(backoff) => {}
                        }
                        let range_header = format!("bytes={offset}-");
                        let send_fut = crate::state::HTTP_CLIENT_PLAYBACK
                            .get(&url)
                            .header("Range", &range_header)
                            .send();
                        // The playback client has no request timeout, so race the
                        // send against restart/cancel: a stalled server must not pin
                        // this task past a seek/stop/new-track.
                        let reconnect_resp = tokio::select! {
                            biased;
                            _ = writer.wait_for_restart_or_cancel() => {
                                if writer.is_cancelled() {
                                    return;
                                }
                                continue 'outer; // seek supersedes the reconnect
                            }
                            result = send_fut => result,
                        };
                        match reconnect_resp {
                            Ok(r) if r.status() == reqwest::StatusCode::PARTIAL_CONTENT => {
                                http_requests += 1;
                                crate::vprintln!(
                                    "[STREAM] Reconnected at byte {offset} (attempt {reconnect_attempts})"
                                );
                                break 'reconnect r.bytes_stream();
                            }
                            Ok(r) if r.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE => {
                                // offset is at/past EOF: all bytes are already buffered.
                                writer.finish();
                                return;
                            }
                            Ok(r) => {
                                crate::vprintln3!(
                                    "[STREAM] DOWNLOAD DIED: reconnect status {} at byte {offset}",
                                    r.status()
                                );
                                writer
                                    .finish_with_error(format!("reconnect status: {}", r.status()));
                                return;
                            }
                            Err(send_err) => {
                                crate::vprintln!(
                                    "[STREAM] reconnect attempt {reconnect_attempts} failed: {send_err}"
                                );
                                continue 'reconnect;
                            }
                        }
                    };
                    continue; // inner loop: new stream, buffer + offset intact
                }
                None => {
                    break;
                }
            }
        }

        // Check for restart before finishing
        if writer.has_restart_pending() {
            continue 'outer;
        }

        // If we downloaded from byte 0, the entire file is in the buffer.
        if stream_offset == 0 {
            writer.finish();
            break;
        }

        // Partial download (Range restart) reached EOF. The buffer covers
        // [base_offset..EOF]. Mark finished so the decode thread sees EOF
        // and emits "completed". If a backward seek needs data before
        // base_offset, buffer.rs clears `finished` and requests a restart.
        writer.finish();
        crate::vprintln2!(
            "[STREAM] Partial EOF (base={}). Waiting for restart or cancel.",
            stream_offset
        );
        loop {
            writer.wait_for_restart_or_cancel().await;
            if writer.is_cancelled() {
                return;
            }
            if writer.has_restart_pending() {
                break; // will continue 'outer
            }
        }
        continue 'outer;
    }

    let total_ms = download_start.elapsed().as_secs_f64() * 1000.0;
    crate::vprintln!(
        "[STREAM] Complete | {} | {} HTTP requests ({} Range restarts)",
        format_ms(total_ms),
        http_requests,
        range_restarts
    );
}
