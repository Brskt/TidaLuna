use crate::bandwidth::TrafficClass;
use crate::decrypt::FlacDecryptor;
use crate::state::{
    CURRENT_TRACK, GOVERNOR, HTTP_CLIENT, PRELOAD_STATE, PreloadedTrack, TrackInfo,
};
use crate::streaming_buffer::StreamingBufferWriter;
use futures_util::StreamExt;

pub async fn fetch_and_decrypt(url: &str, key: &str) -> anyhow::Result<Vec<u8>> {
    let start = std::time::Instant::now();
    let resp = HTTP_CLIENT.get(url).send().await?;

    if !resp.status().is_success() {
        anyhow::bail!("Upstream status: {}", resp.status());
    }

    let decryptor = FlacDecryptor::new(key)?;
    let mut stream = resp.bytes_stream();
    let mut offset = 0u64;
    let mut buffer = Vec::new();

    while let Some(item) = stream.next().await {
        let chunk = item?;

        GOVERNOR
            .acquire(TrafficClass::Preload, chunk.len() as u32)
            .await;

        let decrypted = decryptor.decrypt_chunk(&chunk, offset)?;
        offset += chunk.len() as u64;
        buffer.extend_from_slice(&decrypted);
    }

    let elapsed = start.elapsed().as_secs_f64();
    let rate_mbps = (offset as f64 * 8.0) / (elapsed * 1_000_000.0);
    eprintln!(
        "[FETCH]  {:.1} MB in {:.1}s ({:.1} Mbps)",
        offset as f64 / 1_048_576.0,
        elapsed,
        rate_mbps
    );

    Ok(buffer)
}

pub async fn start_preload(track: TrackInfo) {
    cancel_preload().await;

    let handle = tokio::spawn(async move {
        if track.key.is_empty() || track.url.is_empty() {
            return;
        }

        eprintln!("[PRELOAD] Starting preload for next track");
        match fetch_and_decrypt(&track.url, &track.key).await {
            Ok(data) => {
                if !data.is_empty() {
                    let mut lock = PRELOAD_STATE.lock().await;
                    lock.data = Some(PreloadedTrack { track, data });
                }
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
}

pub async fn next_preloaded_track() -> Option<TrackInfo> {
    let current = {
        let lock = CURRENT_TRACK.lock().unwrap();
        lock.clone()
    };

    let lock = PRELOAD_STATE.lock().await;
    let candidate = lock.data.as_ref().map(|d| d.track.clone());

    match (current, candidate) {
        (Some(curr), Some(next)) if curr == next => None,
        (_, next) => next,
    }
}

pub async fn take_preloaded_if_match(track: &TrackInfo) -> Option<PreloadedTrack> {
    let mut lock = PRELOAD_STATE.lock().await;
    if let Some(data) = lock.data.as_ref()
        && data.track == *track
    {
        return lock.data.take();
    }
    None
}

/// Compute `(seek_padding, warmup_past_target)` based on the track's bitrate.
///
/// - `seek_padding`: 5 seconds of audio, clamped to [512 KB, 2 MB].
///   Covers the decoder's probe-back (~300-400 KB observed).
/// - `warmup_past_target`: 2 seconds of audio, clamped to [256 KB, 512 KB].
///   Data downloaded after the seek target in ungoverned warmup mode.
///   The governor boost (30×) takes over immediately after.
///
/// Returns (512 KB, 256 KB) when bitrate is unknown (0).
///
/// `bitrate_bps` is in **bytes per second** (total_len / duration), matching
/// the unit stored in `BufferProgress::bitrate_bps`.
fn seek_params(bitrate_bps: u64) -> (u64, u64) {
    const PAD_FLOOR: u64 = 512 * 1024;
    const PAD_CEIL: u64 = 2 * 1024 * 1024;
    const PAD_SECS: u64 = 5;

    const PT_FLOOR: u64 = 256 * 1024;
    const PT_CEIL: u64 = 512 * 1024;
    const PT_SECS: u64 = 2;

    if bitrate_bps == 0 {
        return (PAD_FLOOR, PT_FLOOR);
    }
    let padding = (bitrate_bps * PAD_SECS).clamp(PAD_FLOOR, PAD_CEIL);
    let past_target = (bitrate_bps * PT_SECS).clamp(PT_FLOOR, PT_CEIL);
    (padding, past_target)
}

pub fn start_streaming_download(
    resp: reqwest::Response,
    url: String,
    key: String,
    writer: StreamingBufferWriter,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let download_start = std::time::Instant::now();
        let decryptor = match FlacDecryptor::new(&key) {
            Ok(d) => d,
            Err(e) => {
                writer.finish_with_error(format!("decrypt init failed: {e}"));
                return;
            }
        };

        const BATCH_SIZE: usize = 128 * 1024; // 128 KB

        let mut current_resp = Some(resp);
        let mut decrypt_calls = 0u32;
        let mut decrypt_total_ms = 0.0f64;
        let mut range_restarts: u32 = 0;
        let mut warmup = false;
        let mut warmup_bytes: u64 = 0;
        let mut warmup_first_logged = false;
        let mut warmup_end_offset: u64 = 0; // absolute file offset to reach
        let mut warmup_past_target: u64 = 0;
        let mut warmup_start: Option<std::time::Instant> = None;
        let mut warmup_headers_at: Option<std::time::Instant> = None;

        'outer: loop {
            // Determine the stream source: initial response or Range request
            let (stream_resp, stream_offset) = if let Some(r) = current_resp.take() {
                (r, 0u64)
            } else {
                // A restart was requested — get the target and padding flag
                let (mut target, mut skip_padding) = match writer.take_restart_target() {
                    Some(t) => t,
                    None => break, // no restart pending, we're done
                };

                let restart_t0 = std::time::Instant::now();

                // Debounce: wait 20ms then check if a newer seek target arrived
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                if let Some((newer, s)) = writer.take_restart_target() {
                    target = newer;
                    skip_padding = s;
                }
                eprintln!(
                    "[NET]    restart: debounce done +{:.0}ms",
                    restart_t0.elapsed().as_secs_f64() * 1000.0
                );

                if writer.is_cancelled() {
                    eprintln!("[STREAM] Download cancelled");
                    return;
                }

                let (seek_padding, wpt) = seek_params(writer.bitrate_bps());

                if !skip_padding {
                    warmup = true;
                    warmup_bytes = 0;
                    warmup_first_logged = false;
                    warmup_past_target = wpt;
                    warmup_end_offset = target + wpt;
                } else {
                    warmup = false;
                }

                // Pad before target so the decoder's probe-back (~300KB)
                // lands within the buffer instead of triggering a second restart.
                // Skip padding for stale cache continuations (data already present).
                let padded_start = if skip_padding {
                    target
                } else {
                    target.saturating_sub(seek_padding)
                };
                eprintln!(
                    "[STREAM] Range restart at byte {padded_start} (target={target}, pad={}KB{})",
                    (target - padded_start) / 1024,
                    if skip_padding { ", cache continue" } else { "" }
                );
                let range_header = format!("bytes={padded_start}-");
                let send_t0 = std::time::Instant::now();
                let send_fut = HTTP_CLIENT.get(&url).header("Range", &range_header).send();
                let range_resp = tokio::select! {
                    biased;
                    _ = writer.wait_for_restart_or_cancel() => {
                        eprintln!(
                            "[NET]    restart: cancelled during send ({:.0}ms)",
                            send_t0.elapsed().as_secs_f64() * 1000.0
                        );
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
                eprintln!(
                    "[NET]    restart: TTFB {:.0}ms (total +{:.0}ms)",
                    send_t0.elapsed().as_secs_f64() * 1000.0,
                    restart_t0.elapsed().as_secs_f64() * 1000.0
                );
                if warmup {
                    let now = std::time::Instant::now();
                    warmup_headers_at = Some(now);
                    warmup_start = Some(now);
                }

                let status = range_resp.status();
                if status == reqwest::StatusCode::PARTIAL_CONTENT {
                    // 206 — server honored the Range
                    writer.reset_for_range(padded_start);
                    range_restarts += 1;
                    (range_resp, padded_start)
                } else if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
                    // 416 — offset beyond file, finish
                    writer.finish();
                    break;
                } else if status.is_success() {
                    // 200 — server ignored Range, restart from beginning
                    eprintln!("[STREAM] Server ignored Range header, restarting from byte 0");
                    writer.reset_for_range(0);
                    (range_resp, 0u64)
                } else {
                    writer.finish_with_error(format!("range request status: {status}"));
                    return;
                }
            };

            let mut stream = stream_resp.bytes_stream();
            let mut offset = stream_offset;
            let mut pending = Vec::with_capacity(BATCH_SIZE);
            let mut pending_offset = stream_offset;

            loop {
                let item = tokio::select! {
                    biased;
                    _ = writer.wait_for_restart_or_cancel() => {
                        if writer.is_cancelled() {
                            eprintln!("[STREAM] Download cancelled");
                            return;
                        }
                        continue 'outer;
                    }
                    item = stream.next() => match item {
                        Some(item) => item,
                        None => break,
                    },
                };

                match item {
                    Ok(chunk) => {
                        if !warmup {
                            GOVERNOR
                                .acquire(TrafficClass::Playback, chunk.len() as u32)
                                .await;
                        }

                        if warmup {
                            // Per-chunk streaming: decrypt and write immediately
                            // until we've fed enough data for the decoder to start.
                            let chunk_offset = offset;
                            let chunk_len = chunk.len();
                            offset += chunk_len as u64;
                            let mut buf = chunk.to_vec();
                            let ds = std::time::Instant::now();
                            match decryptor.decrypt_in_place(&mut buf, chunk_offset) {
                                Ok(()) => {
                                    let decrypt_elapsed = ds.elapsed();
                                    let decrypt_ms = decrypt_elapsed.as_secs_f64() * 1000.0;
                                    decrypt_total_ms += decrypt_ms;
                                    decrypt_calls += 1;
                                    writer.write(&buf);
                                    warmup_bytes += chunk_len as u64;
                                    if !warmup_first_logged {
                                        warmup_first_logged = true;
                                        if let Some(ha) = warmup_headers_at {
                                            let headers_to_body_ms =
                                                ds.duration_since(ha).as_secs_f64() * 1000.0;
                                            eprintln!(
                                                "[NET]    restart: headers→body +{:.0}ms | decrypt +{:.0}ms ({} B)",
                                                headers_to_body_ms, decrypt_ms, chunk_len
                                            );
                                        }
                                    }
                                    if offset >= warmup_end_offset {
                                        if let Some(ref ws) = warmup_start {
                                            eprintln!(
                                                "[NET]    warmup done: {}KB in {:.0}ms (past target: {}KB)",
                                                warmup_bytes / 1024,
                                                ws.elapsed().as_secs_f64() * 1000.0,
                                                warmup_past_target / 1024
                                            );
                                        }
                                        warmup = false;
                                    }
                                }
                                Err(e) => {
                                    writer.finish_with_error(format!("decrypt error: {e}"));
                                    return;
                                }
                            }
                        } else {
                            // Normal batch mode
                            if pending.is_empty() {
                                pending_offset = offset;
                            }
                            offset += chunk.len() as u64;
                            pending.extend_from_slice(&chunk);

                            if pending.len() >= BATCH_SIZE {
                                let ds = std::time::Instant::now();
                                match decryptor.decrypt_in_place(&mut pending, pending_offset) {
                                    Ok(()) => {
                                        decrypt_total_ms += ds.elapsed().as_secs_f64() * 1000.0;
                                        decrypt_calls += 1;
                                        writer.write(&pending);
                                        pending.clear();

                                        writer.wait_if_buffer_full().await;
                                        if writer.has_restart_pending() {
                                            continue 'outer;
                                        }
                                    }
                                    Err(e) => {
                                        writer.finish_with_error(format!("decrypt error: {e}"));
                                        return;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        writer.finish_with_error(format!("network error: {e}"));
                        return;
                    }
                }
            }

            // Flush remaining data
            if !pending.is_empty() {
                let ds = std::time::Instant::now();
                match decryptor.decrypt_in_place(&mut pending, pending_offset) {
                    Ok(()) => {
                        decrypt_total_ms += ds.elapsed().as_secs_f64() * 1000.0;
                        decrypt_calls += 1;
                        writer.write(&pending);
                    }
                    Err(e) => {
                        writer.finish_with_error(format!("decrypt error: {e}"));
                        return;
                    }
                }
            }

            // Check for restart before finishing
            if writer.has_restart_pending() {
                continue 'outer;
            }

            writer.finish();
            break;
        }

        let total_ms = download_start.elapsed().as_secs_f64() * 1000.0;
        let net_ms = total_ms - decrypt_total_ms;
        eprintln!(
            "[STREAM] Complete | {:.0}ms (net: {:.0}ms, decrypt: {:.0}ms) | {} decrypt calls | {} restarts",
            total_ms, net_ms, decrypt_total_ms, decrypt_calls, range_restarts
        );
    })
}
