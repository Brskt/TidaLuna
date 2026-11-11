use super::{open_cipher_sink, park_ciphertext};
use crate::audio::decrypt::FlacDecryptor;
use crate::player::buffer::{DownloadFailure, RamBufferWriter};
use crate::util::fmt::format_ms;
use futures_util::StreamExt;

/// Start a streaming download into a RamBufferWriter.
/// Handles decryption, governor rate limiting, and Range restarts.
pub fn start_download(
    resp: reqwest::Response,
    url: String,
    key: String,
    writer: RamBufferWriter,
) -> tokio::task::JoinHandle<()> {
    // Taken from the buffer rather than passed in: the buffer is what gets handed from a
    // staged track to the one being listened to, and the token has to travel with it.
    let cancel = writer.cancel_token();
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

pub(in crate::audio) async fn download_stream(
    resp: reqwest::Response,
    url: String,
    key: String,
    writer: RamBufferWriter,
) {
    // Captured once: it names which track this task belongs to, and for a staged download
    // it is also the credential every reconnect uses. A playback download re-reads its own
    // through `owner.fetch_url`, since that one can be refreshed under us.
    let task_canonical_id = crate::player::canonical_track_id(&url);
    let download_start = std::time::Instant::now();
    let decryptor = if key.is_empty() {
        None
    } else {
        match FlacDecryptor::new(&key) {
            Ok(d) => Some(d),
            Err(e) => {
                writer.finish_with_error(
                    DownloadFailure::Source,
                    format!("decrypt init failed: {e}"),
                );
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
            // A seek restart starts a fresh stream: clear the reconnect
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

            // One read for the whole decision: the pool a reconnect uses and the credential
            // it carries have to come from the same owner, and adoption can land between
            // two reads of it.
            let owner = writer.owner();
            let Some(fetch_url) = owner.fetch_url(&task_canonical_id, &url) else {
                // The retained track is no longer ours: this task is stale.
                return;
            };
            let send_fut = owner
                .http_client()
                .get(&fetch_url)
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
                            let text = crate::util::network_error_text(e);
                            crate::vprintln3!(
                                "[STREAM] DOWNLOAD DIED: range request failed at byte {target} | restarts={range_restarts}: {text}"
                            );
                            writer.finish_with_error(
                                DownloadFailure::Network,
                                format!("range request failed: {text}"),
                            );
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
                writer.finish_with_error(
                    DownloadFailure::Source,
                    format!("range request status: {status}"),
                );
                return;
            }
        };

        // Staging follows the buffer's cacheability: only a stream from byte 0 can be
        // cached; a Range-based one stages nothing. Re-opening here also covers a
        // server that ignored Range and put us back at 0.
        let mut cipher_sink = if stream_offset == 0 {
            open_cipher_sink(&crate::player::canonical_track_id(&url))
        } else {
            None
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
                        // The owner is re-read per chunk: an adopted preload starts charging
                        // the listener's bucket from the chunk after the handover, where the
                        // fixed preload rate cannot keep a hi-res track fed.
                        _ = writer.governor().acquire(writer.owner().traffic_class(), chunk.len() as u32) => {}
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
                            // `chunk` is still ciphertext: decryption ran on the copy
                            // in `decrypt_buf`. A discarded write means a restart is
                            // imminent: the file must not diverge from the buffer.
                            cipher_sink = match cipher_sink.take() {
                                Some(sink) if written => sink.append(&chunk).await,
                                _ => None,
                            };
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
                            writer.finish_with_error(
                                DownloadFailure::Source,
                                format!("decrypt error: {e}"),
                            );
                            return;
                        }
                    }
                }
                Some(Err(e)) => {
                    // Transient transport error (idle connection died on a long
                    // pause, or a blip): reconnect at `offset` and keep appending -
                    // [base..offset] stays valid, and the decode blocks at the frontier
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
                            writer.finish_with_error(
                                DownloadFailure::Network,
                                format!("network error after {MAX_RECONNECTS} reconnects: {e}"),
                            );
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
                        // One read, for the same reason as the restart path above.
                        let owner = writer.owner();
                        let Some(fetch_url) = owner.fetch_url(&task_canonical_id, &url) else {
                            // The retained track is no longer ours. This task is stale.
                            return;
                        };
                        let send_fut = owner
                            .http_client()
                            .get(&fetch_url)
                            .header("Range", &range_header)
                            .send();
                        // The playback client has no request timeout: race the
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
                                // offset is at/past EOF: all bytes are already
                                // buffered: this download is complete and
                                // cacheable just like a clean EOF.
                                park_ciphertext(&writer, cipher_sink.take(), stream_offset).await;
                                writer.finish();
                                return;
                            }
                            Ok(r) => {
                                crate::vprintln3!(
                                    "[STREAM] DOWNLOAD DIED: reconnect status {} at byte {offset}",
                                    r.status()
                                );
                                writer.finish_with_error(
                                    DownloadFailure::Source,
                                    format!("reconnect status: {}", r.status()),
                                );
                                return;
                            }
                            Err(send_err) => {
                                // Same pair as the preload twin above. Worth naming here in
                                // particular: this loop re-derives `fetch_url` every attempt,
                                // so a re-signed credential can move the download to another
                                // edge mid-reconnect, and the host is the only thing on the
                                // line that would say so.
                                crate::vprintln!(
                                    "[STREAM] reconnect attempt {reconnect_attempts} to {} failed: {}",
                                    crate::util::redact_url_query(&fetch_url),
                                    crate::util::network_error_text(send_err)
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
            park_ciphertext(&writer, cipher_sink.take(), stream_offset).await;
            writer.finish();
            break;
        }

        // Partial download (Range restart) reached EOF. The buffer covers
        // [base_offset..EOF]. Mark finished: the decode thread sees EOF and
        // emits "completed". If a backward seek needs data before
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
