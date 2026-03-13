use crate::bandwidth::TrafficClass;
use crate::decrypt::FlacDecryptor;
use crate::player::buffer::RamBufferWriter;
use crate::state::{GOVERNOR, HTTP_CLIENT, PRELOAD_STATE, PreloadedTrack, TrackInfo};
use futures_util::StreamExt;

const PRELOAD_MAX_BYTES: usize = 32 * 1024 * 1024; // 32 MB

fn format_ms(ms: f64) -> String {
    if ms < 1.0 {
        format!("{:.0}µs", ms * 1000.0)
    } else {
        format!("{:.0}ms", ms)
    }
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * KB;
    if (bytes as f64) >= MB {
        format!("{:.1} MB", bytes as f64 / MB)
    } else {
        format!("{:.0} KB", bytes as f64 / KB)
    }
}

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

    while let Some(item) = stream.next().await {
        let chunk = item?;

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
                eprintln!("Preload failed: {}", e);
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
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
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

                // Brief debounce to coalesce rapid seeks
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                let target = writer.take_restart_target().unwrap_or(target);

                if writer.is_cancelled() {
                    return;
                }

                let range_header = format!("bytes={target}-");
                crate::vprintln!("[STREAM] Range restart at byte {target}");

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
                        crate::vprintln!("[STREAM] Restart aborted (new restart pending) after {}", format_ms(restart_t0.elapsed().as_secs_f64() * 1000.0));
                        continue 'outer;
                    }
                    result = send_fut => {
                        match result {
                            Ok(r) => r,
                            Err(e) => {
                                writer.finish_with_error(format!("range request failed: {e}"));
                                return;
                            }
                        }
                    }
                };

                let cdn = crate::player::cdn_cache_status(&range_resp);
                let ttfb = format_ms(restart_t0.elapsed().as_secs_f64() * 1000.0);
                crate::vprintln!(
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
                    crate::vprintln!(
                        "[STREAM] Server ignored Range header, restarting from byte 0"
                    );
                    writer.reset_for_range(0);
                    (range_resp, 0u64)
                } else {
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
                        crate::vprintln!(
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
                                let written = writer.write_counted(&decrypt_buf);
                                chunk_count += 1;
                                bytes_since_restart += chunk.len() as u64;

                                // Log first chunk + progress every 512KB
                                if chunk_count == 1 {
                                    crate::vprintln!(
                                        "[STREAM] First chunk: {}B at {}",
                                        chunk.len(),
                                        format_ms(stream_start.elapsed().as_secs_f64() * 1000.0),
                                    );
                                }
                                if !written {
                                    crate::vprintln!(
                                        "[STREAM] Write DISCARDED (restart pending) at {}KB",
                                        bytes_since_restart / 1024
                                    );
                                }
                                if bytes_since_restart - last_progress_bytes >= 512 * 1024 {
                                    crate::vprintln!(
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
                        writer.finish_with_error(format!("network error: {e}"));
                        return;
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
            crate::vprintln!(
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
    })
}
