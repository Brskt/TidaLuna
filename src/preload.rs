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
        let mut warmup = false;
        let mut warmup_start: Option<std::time::Instant> = None;

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

                // Debounce: wait 20ms then check if a newer seek target arrived
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                if let Some((newer, s)) = writer.take_restart_target() {
                    target = newer;
                    skip_padding = s;
                }

                if writer.is_cancelled() {
                    eprintln!("[STREAM] Download cancelled");
                    return;
                }

                if !skip_padding {
                    warmup = true;
                    warmup_start = Some(std::time::Instant::now());
                }

                // Pad before target so the decoder's probe-back (~300KB)
                // lands within the buffer instead of triggering a second restart.
                // Skip padding for stale cache continuations (data already present).
                const SEEK_PADDING: u64 = 512 * 1024;
                let padded_start = if skip_padding {
                    target
                } else {
                    target.saturating_sub(SEEK_PADDING)
                };
                eprintln!(
                    "[STREAM] Range restart at byte {padded_start} (target={target}, pad={}KB{})",
                    (target - padded_start) / 1024,
                    if skip_padding { ", cache continue" } else { "" }
                );
                let range_header = format!("bytes={padded_start}-");
                let range_resp = match HTTP_CLIENT
                    .get(&url)
                    .header("Range", &range_header)
                    .send()
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        writer.finish_with_error(format!("range request failed: {e}"));
                        return;
                    }
                };

                let status = range_resp.status();
                if status == reqwest::StatusCode::PARTIAL_CONTENT {
                    // 206 — server honored the Range
                    writer.reset_for_range(padded_start);
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

            while let Some(item) = stream.next().await {
                if writer.is_cancelled() {
                    eprintln!("[STREAM] Download cancelled");
                    return;
                }
                if writer.has_restart_pending() {
                    continue 'outer;
                }

                match item {
                    Ok(chunk) => {
                        GOVERNOR
                            .acquire(TrafficClass::Playback, chunk.len() as u32)
                            .await;

                        if warmup {
                            // Per-chunk streaming: decrypt and write immediately
                            let chunk_offset = offset;
                            offset += chunk.len() as u64;
                            let mut buf = chunk.to_vec();
                            let ds = std::time::Instant::now();
                            match decryptor.decrypt_in_place(&mut buf, chunk_offset) {
                                Ok(()) => {
                                    decrypt_total_ms += ds.elapsed().as_secs_f64() * 1000.0;
                                    decrypt_calls += 1;
                                    writer.write(&buf);
                                    if let Some(ws) = warmup_start.take() {
                                        eprintln!(
                                            "[STREAM] First write: {:.0}ms ({} B)",
                                            ws.elapsed().as_secs_f64() * 1000.0,
                                            buf.len()
                                        );
                                    }
                                    warmup = false;
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
            "[STREAM] Complete | {:.0}ms (net: {:.0}ms, decrypt: {:.0}ms) | {} decrypt calls",
            total_ms, net_ms, decrypt_total_ms, decrypt_calls
        );
    })
}
