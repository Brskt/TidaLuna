use super::open_cipher_sink;
use crate::audio::bandwidth::GovernorHandle;
use crate::audio::decrypt::FlacDecryptor;
use crate::player::buffer::DownloadOwner;
use crate::state::MediaResponse;
use crate::util::fmt::format_bytes;

/// A complete in-RAM fetch: plaintext for the decoder, plus the ciphertext the disk
/// cache stores. `None` when staging failed or the fetch was capped - caching is
/// best-effort and must never fail a load.
pub struct FetchedTrack {
    pub data: Vec<u8>,
    pub ciphertext: Option<(tempfile::NamedTempFile, u64)>,
}

/// `owner` decides which bucket of `governor` pays; this path has no streaming buffer to carry
/// either, and a load with no `Content-Length` falls back here too. `resp` is a response already
/// in hand, its success status checked by the caller: issuing a second GET holds the first open
/// with its body unread, and an HTTP/1.1 connection in that state is unusable until it drops.
pub(in crate::audio) async fn fetch_and_decrypt_inner(
    url: &str,
    key: &str,
    max_bytes: Option<usize>,
    owner: DownloadOwner,
    resp: MediaResponse,
    governor: &GovernorHandle,
) -> anyhow::Result<Option<FetchedTrack>> {
    let start = std::time::Instant::now();

    let decryptor = if key.is_empty() {
        None
    } else {
        Some(FlacDecryptor::new(key)?)
    };
    let mut stream = resp.body();
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
                    // On the owner's own pool, like the reconnects in `download_stream`. This
                    // path serves the active load as well as a preload now, and the two clients
                    // exist precisely to keep a listener's stream from queueing behind staged
                    // traffic; a reconnect that switched pools would hand it straight back.
                    match owner
                        .http_client()
                        .get(url)
                        .starting_at(offset)
                        .send()
                        .await
                    {
                        Ok(r) if r.status() == reqwest::StatusCode::PARTIAL_CONTENT => {
                            crate::vprintln!(
                                "[PRELOAD] Reconnected at byte {offset} (attempt {reconnect_attempts})"
                            );
                            break 'reconnect r.body();
                        }
                        Ok(r) if r.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE => {
                            break 'download;
                        }
                        Ok(r) => anyhow::bail!("reconnect status: {}", r.status()),
                        Err(send_err) => {
                            // Two redactions on two values, as `net_fetch.rs` does: the url
                            // names the edge that refused, with its signed query cut, while
                            // the error's own copy of that url is already gone.
                            crate::vprintln!(
                                "[PRELOAD] reconnect attempt {reconnect_attempts} to {} failed: {}",
                                crate::util::redact_url_query(url),
                                send_err
                            );
                            continue 'reconnect;
                        }
                    }
                };
                continue 'download; // new stream, buffer + offset intact
            }
        };

        reconnect_attempts = 0; // received data; reset the retry budget

        governor
            .acquire(owner.traffic_class(), chunk.len() as u32)
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

/// The uncapped whole-copy fetch, for the load path that has no `Content-Length` to build a
/// streaming buffer from. Its bytes are the listener's: the caller is waiting to hear them,
/// which is also why it hands over the response it already opened, and checked for success,
/// rather than leaving it to idle against a second one.
pub async fn fetch_and_decrypt(
    url: &str,
    key: &str,
    opened: MediaResponse,
    governor: &GovernorHandle,
) -> anyhow::Result<FetchedTrack> {
    match fetch_and_decrypt_inner(url, key, None, DownloadOwner::Playback, opened, governor).await?
    {
        Some(fetched) => Ok(fetched),
        None => anyhow::bail!("unexpected capped fetch in uncapped mode"),
    }
}
