use crate::decrypt::FlacDecryptor;
use crate::state::{CURRENT_TRACK, PRELOAD_STATE, PreloadedTrack, TrackInfo};
use crate::streaming_buffer::StreamingBufferWriter;
use futures_util::StreamExt;

pub async fn fetch_and_decrypt(url: &str, key: &str) -> anyhow::Result<Vec<u8>> {
    let client = reqwest::Client::new();
    let resp = client.get(url).send().await?;

    if !resp.status().is_success() {
        anyhow::bail!("Upstream status: {}", resp.status());
    }

    let decryptor = FlacDecryptor::new(key)?;
    let mut stream = resp.bytes_stream();
    let mut offset = 0u64;
    let mut buffer = Vec::new();

    while let Some(item) = stream.next().await {
        let chunk = item?;
        let decrypted = decryptor.decrypt_chunk(&chunk, offset)?;
        offset += chunk.len() as u64;
        buffer.extend_from_slice(&decrypted);
    }

    Ok(buffer)
}

pub async fn start_preload(track: TrackInfo) {
    cancel_preload().await;

    let handle = tokio::spawn(async move {
        if track.key.is_empty() || track.url.is_empty() {
            return;
        }

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
    if let Some(data) = lock.data.as_ref() {
        if data.track == *track {
            return lock.data.take();
        }
    }
    None
}

pub fn start_streaming_download(
    resp: reqwest::Response,
    key: String,
    writer: StreamingBufferWriter,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let decryptor = match FlacDecryptor::new(&key) {
            Ok(d) => d,
            Err(e) => {
                writer.finish_with_error(format!("decrypt init failed: {e}"));
                return;
            }
        };

        let mut stream = resp.bytes_stream();
        let mut offset = 0u64;

        while let Some(item) = stream.next().await {
            if writer.is_cancelled() {
                eprintln!("[STREAM] Download cancelled");
                return;
            }

            match item {
                Ok(chunk) => match decryptor.decrypt_chunk(&chunk, offset) {
                    Ok(decrypted) => {
                        offset += chunk.len() as u64;
                        writer.write(&decrypted);
                    }
                    Err(e) => {
                        writer.finish_with_error(format!("decrypt error: {e}"));
                        return;
                    }
                },
                Err(e) => {
                    writer.finish_with_error(format!("network error: {e}"));
                    return;
                }
            }
        }

        writer.finish();
        eprintln!("[STREAM] Download complete ({} bytes decrypted)", offset);
    })
}
