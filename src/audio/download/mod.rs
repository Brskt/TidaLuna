use crate::audio::bandwidth::TrafficClass;
use crate::player::buffer::{DownloadOwner, RamBufferWriter};
use crate::state::HTTP_CLIENT_PRELOAD;

mod stream;
mod whole;

pub(crate) use stream::start_download;
pub(crate) use whole::fetch_and_decrypt;
pub(in crate::audio) use whole::fetch_and_decrypt_inner;

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
    crate::player::refreshed_fetch_url(task_canonical_id, retained.retained().map(|r| &r.track))
}

/// What a download's current owner settles: which queue governs its bytes, which pool it
/// reconnects on, and where it finds a credential to reconnect with.
///
/// The two owners differ in one fact, and everything here follows from it: whether the track
/// being fetched is the one `CURRENT_TRACK` names.
impl DownloadOwner {
    fn traffic_class(self) -> TrafficClass {
        match self {
            Self::Playback => TrafficClass::Playback,
            Self::Preload => TrafficClass::Preload,
        }
    }

    /// A staged download reconnects on the pool it started on. The two clients exist to
    /// keep preload traffic off the playback pool's contention, and a reconnect that
    /// switched pools would hand it straight back.
    fn http_client(self) -> &'static reqwest::Client {
        match self {
            Self::Playback => &crate::state::HTTP_CLIENT_PLAYBACK,
            Self::Preload => &HTTP_CLIENT_PRELOAD,
        }
    }

    /// The credential to reconnect with, or `None` when the task is stale and must stop.
    ///
    /// A staged download uses the url captured at spawn, re-staging going through
    /// `cancel_preload` so a fresh url arrives with a fresh task. Once adopted, the track IS
    /// current and a same-track re-assert can re-sign this task's url underneath it, which is
    /// when the global becomes the right place to read.
    fn fetch_url(self, task_canonical_id: &str, captured: &str) -> Option<String> {
        match self {
            Self::Playback => current_fetch_url(task_canonical_id),
            Self::Preload => Some(captured.to_string()),
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/audio/download.rs"]
mod tests;
