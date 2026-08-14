use crate::audio::bandwidth::TrafficClass;
use crate::audio::decrypt::FlacDecryptor;
use crate::player::buffer::RamBufferWriter;
use crate::state::{GOVERNOR, HTTP_CLIENT, PRELOAD_STATE, PreloadedTrack, TrackInfo};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

const PRELOAD_MAX_BYTES: usize = 32 * 1024 * 1024; // 32 MB

use crate::util::fmt::{format_bytes, format_ms};

/// A complete in-RAM fetch: plaintext for the decoder, plus the ciphertext the disk
/// cache stores. `None` when staging failed or the fetch was capped - caching is
/// best-effort and must never fail a load.
pub struct FetchedTrack {
    pub data: Vec<u8>,
    pub ciphertext: Option<(tempfile::NamedTempFile, u64)>,
}

async fn fetch_and_decrypt_inner(
    url: &str,
    key: &str,
    max_bytes: Option<usize>,
) -> anyhow::Result<Option<FetchedTrack>> {
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
    // A reconnect resumes at `offset` and appends; the staged bytes stay linear
    // from zero - no equivalent of download_stream's Range restart exists here.
    let mut cipher_sink = open_cipher_sink(&crate::player::canonical_track_id(url));

    'download: loop {
        let item = match stream.next().await {
            Some(item) => item,
            None => break 'download,
        };
        let chunk = match item {
            Ok(chunk) => chunk,
            Err(e) => {
                // Idle connection died on a long pause: reconnect at `offset` and keep
                // appending - the decrypt offset stays aligned and resumed bytes decrypt
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

        // Stage the CDN's bytes before decrypting the copy below.
        if let Some(sink) = cipher_sink.take() {
            cipher_sink = sink.append(&chunk).await;
        }

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

    let ciphertext = match cipher_sink {
        Some(sink) => sink.finish().await,
        None => None,
    };

    Ok(Some(FetchedTrack {
        data: buffer,
        ciphertext,
    }))
}

pub async fn fetch_and_decrypt(url: &str, key: &str) -> anyhow::Result<FetchedTrack> {
    match fetch_and_decrypt_inner(url, key, None).await? {
        Some(fetched) => Ok(fetched),
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

        // try_cache_hit serves a cached track before the preload is consulted, so
        // fetching it spends network and a staged copy on bytes nothing reads.
        // next_track stays set: auto-load proceeds as for an over-size track.
        let already_cached = crate::state::AUDIO_CACHE.lock().ok().is_some_and(|c| {
            c.lookup_path(&crate::player::canonical_track_id(&track.url))
                .is_some()
        });
        if already_cached {
            crate::vprintln!("[PRELOAD] Next track is already cached, skipping fetch");
            return;
        }

        crate::vprintln!("[PRELOAD] Starting preload for next track");
        match fetch_and_decrypt_inner(&track.url, &track.key, Some(PRELOAD_MAX_BYTES)).await {
            Ok(Some(fetched)) => {
                if !fetched.data.is_empty() {
                    let mut lock = PRELOAD_STATE.lock().await;
                    if lock.next_track.as_ref() == Some(&track) {
                        lock.data = Some(PreloadedTrack {
                            track,
                            data: fetched.data,
                            ciphertext: fetched.ciphertext,
                        });
                    }
                }
            }
            Ok(None) => {
                // Too large for RAM cache; keep only next_track, letting auto-load proceed.
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

/// How much ciphertext to accumulate before touching the disk.
const STAGING_FLUSH_BYTES: usize = 1024 * 1024;

/// Collects the stream's bytes as they arrive from the CDN, before decryption, so
/// the disk cache stores ciphertext rather than playable audio. Chunks pile up in
/// RAM and reach the disk one `STAGING_FLUSH_BYTES` block at a time on a blocking
/// worker: reqwest yields 16-64 KB at a time, and writing each inline would put
/// thousands of blocking writes on the runtime thread driving the download.
struct CipherSink {
    file: tempfile::NamedTempFile,
    len: u64,
    pending: Vec<u8>,
}

impl CipherSink {
    /// By value because a flush moves the file onto a blocking worker. `None` means
    /// staging failed and the caller must stop staging.
    async fn append(mut self, chunk: &[u8]) -> Option<Self> {
        self.pending.extend_from_slice(chunk);
        if self.pending.len() < STAGING_FLUSH_BYTES {
            return Some(self);
        }
        self.flush().await
    }

    async fn flush(mut self) -> Option<Self> {
        if self.pending.is_empty() {
            return Some(self);
        }
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            self.file.write_all(&self.pending).ok()?;
            self.len += self.pending.len() as u64;
            self.pending.clear();
            Some(self)
        })
        .await
        .ok()?
    }

    /// Flush the tail and hand over the staging file, or `None` if nothing was ever
    /// written. The one gate on emptiness, sparing every caller its own: a zero-length
    /// entry would be stored, read back, rejected and deleted for nothing.
    async fn finish(self) -> Option<(tempfile::NamedTempFile, u64)> {
        let sink = self.flush().await?;
        (sink.len > 0).then_some((sink.file, sink.len))
    }
}

/// Staging sink for the ciphertext, or `None` if this track should not be staged.
/// Caching is best-effort: every failure here disables it silently. Refuses an
/// already-indexed track and a disabled cache: neither could use the result, and
/// both would write a full track only to delete it.
fn open_cipher_sink(track_id: &str) -> Option<CipherSink> {
    let dir = {
        let cache = crate::state::AUDIO_CACHE.lock().ok()?;
        if cache.lookup_path(track_id).is_some() {
            return None;
        }
        cache.staging_dir()?
    };
    // Unlocked: syscalls, and the cache lock sits on every concurrent lookup.
    std::fs::create_dir_all(&dir).ok()?;
    let file = tempfile::NamedTempFile::new_in(&dir).ok()?;
    Some(CipherSink {
        file,
        len: 0,
        pending: Vec::with_capacity(STAGING_FLUSH_BYTES),
    })
}

/// Hand a complete-from-zero download's ciphertext to the cache writer. Every EOF
/// path goes through here because one that forgets silently stops caching instead
/// of failing, which is how the 416-on-reconnect exit was missed. A Range-started
/// stream is not cacheable (the gate is `base_offset == 0`), and it drops the sink.
async fn park_ciphertext(writer: &RamBufferWriter, sink: Option<CipherSink>, stream_offset: u64) {
    if stream_offset != 0 {
        return;
    }
    if let Some(sink) = sink
        && let Some((file, len)) = sink.finish().await
    {
        writer.set_ciphertext(file, len);
    }
}

/// The credential this task must fetch with now, or `None` if the task is stale. Read at each
/// re-fetch rather than captured at start: the signed url is refreshed in place on a same-track
/// re-assert; a captured copy would otherwise expire while the task is still legitimately running.
fn current_fetch_url(task_canonical_id: &str) -> Option<String> {
    let retained = crate::state::CURRENT_TRACK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::player::refreshed_fetch_url(task_canonical_id, retained.as_ref())
}

async fn download_stream(
    resp: reqwest::Response,
    url: String,
    key: String,
    writer: RamBufferWriter,
) {
    // Captured once, only to recognise which track this task belongs to. The url itself is re-read
    // per fetch through `current_fetch_url`, since it can be refreshed under us.
    let task_canonical_id = crate::player::canonical_track_id(&url);
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

            let Some(fetch_url) = current_fetch_url(&task_canonical_id) else {
                // The retained track is no longer ours: this task is stale.
                return;
            };
            let send_fut = crate::state::HTTP_CLIENT_PLAYBACK
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
                            writer.finish_with_error(format!("decrypt error: {e}"));
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
                        let Some(fetch_url) = current_fetch_url(&task_canonical_id) else {
                            // The retained track is no longer ours. This task is stale.
                            return;
                        };
                        let send_fut = crate::state::HTTP_CLIENT_PLAYBACK
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
