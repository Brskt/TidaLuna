pub(crate) mod asio;
pub(crate) mod buffer;
pub(crate) mod cache;
pub(crate) mod dash;
mod declick;
pub(crate) mod ipc;
mod resume;
mod thread;
#[cfg(target_os = "windows")]
mod throttle;
#[cfg(target_os = "windows")]
pub(crate) mod wasapi;

use crate::audio::preload;
use crate::state::{
    AUDIO_CACHE, CURRENT_METADATA, CURRENT_TRACK, GOVERNOR, HTTP_CLIENT_PLAYBACK, TrackInfo,
};
use buffer::RamBuffer;
use futures_util::stream::{self, StreamExt};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(crate) static LOAD_SEQ: AtomicU32 = AtomicU32::new(0);
static EVENT_SEQ: AtomicU32 = AtomicU32::new(0);
#[cfg(target_os = "windows")]
static EXCLUSIVE_STREAM_SEQ: AtomicU32 = AtomicU32::new(0);
#[cfg(target_os = "windows")]
static ASIO_STREAM_SEQ: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, serde::Serialize, Clone)]
pub struct AudioDevice {
    #[serde(rename = "controllableVolume")]
    pub controllable_volume: bool,
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Ready,
    Active,
    Paused,
    Stopped,
    Seeking,
    Idle,
    Completed,
}

impl PlaybackState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
            Self::Seeking => "seeking",
            Self::Idle => "idle",
            Self::Completed => "completed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // ExclusiveModeNotAllowed + Locked are Windows-only
pub enum DeviceErrorKind {
    NotFound,
    FormatNotSupported,
    ExclusiveModeNotAllowed,
    ExclusiveFormatUnsupported,
    Locked,
    Disconnected,
    Unknown,
    AsioDriverNotFound,
    AsioFormatUnsupported,
    AsioInitFailed,
    AsioRateUnsupported,
}

impl DeviceErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "devicenotfound",
            Self::FormatNotSupported => "deviceformatnotsupported",
            Self::ExclusiveModeNotAllowed => "deviceexclusivemodenotallowed",
            Self::ExclusiveFormatUnsupported => "deviceexclusiveformatunsupported",
            Self::Locked => "devicelocked",
            Self::Disconnected => "devicedisconnected",
            Self::Unknown => "deviceunknownerror",
            Self::AsioDriverNotFound => "deviceasiodrivernotfound",
            Self::AsioFormatUnsupported => "deviceasioformatunsupported",
            Self::AsioInitFailed => "deviceasioinitfailed",
            Self::AsioRateUnsupported => "deviceasiorateunsupported",
        }
    }
}

/// `mediaerror` codes the SDK recognizes (its `mediaErrorCodeMap`); any other
/// string degrades to `errorCode: undefined` on the SDK side. Typed so a site
/// can't invent an off-contract code. `file_checksum_mismatch` has no producer here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaErrorCode {
    /// NPO01 - the source couldn't be fetched (HTTP failure, missing segment).
    NoSuchFile,
    /// NPO03 - the source was fetched but can't be read (probe/decode failure).
    UnreadableFile,
}

impl MediaErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoSuchFile => "no_such_file",
            Self::UnreadableFile => "unreadable_file",
        }
    }
}

/// The audio output backend a `player.devices.set` selects. The three modes are
/// mutually exclusive (a radio choice), so a single enum replaces the former
/// `(exclusive, asio)` boolean pair, which could encode the invalid both-true
/// state. `Exclusive`/`Asio` are Windows-only at runtime (the frontend toggles
/// gating them are win32-only); other platforms only ever see `Shared`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Shared output through the OS mixer (cpal).
    Shared,
    /// Exclusive WASAPI: bypasses the OS mixer.
    Exclusive,
    /// ASIO driver output: bypasses the OS mixer.
    Asio,
}

/// Last `MediaFormat` emitted for the committed track (shared-path probe only;
/// ASIO/exclusive loads don't emit one). Re-emitted on a same-track re-assert
/// so the renderer's nulled per-load format snapshot isn't left empty.
#[derive(Debug, Clone, Copy)]
pub struct MediaFormatSnapshot {
    pub codec: &'static str,
    pub sample_rate: u32,
    pub bit_depth: Option<u32>,
    pub channels: u16,
    pub bytes: u64,
}

impl MediaFormatSnapshot {
    pub fn to_event(self) -> PlayerEvent {
        PlayerEvent::MediaFormat {
            codec: self.codec,
            sample_rate: self.sample_rate,
            bit_depth: self.bit_depth,
            channels: self.channels,
            bytes: self.bytes,
        }
    }
}

#[derive(Debug, Clone)]
pub enum PlayerEvent {
    TimeUpdate(f64, u32),
    Duration(f64, u32),
    StateChange(PlaybackState, u32),
    AudioDevices(Vec<AudioDevice>, Option<String>),
    DeviceError(DeviceErrorKind),
    MediaFormat {
        codec: &'static str,
        sample_rate: u32,
        bit_depth: Option<u32>,
        channels: u16,
        bytes: u64,
    },
    Version(&'static str),
    MediaError {
        error: String,
        code: MediaErrorCode,
    },
    MaxConnectionsReached,
    /// Re-arm a retained source into the shared pipeline: flush.rs reloads `track`
    /// at the generation snapshot (skipped if a newer load/stop superseded it),
    /// seeks to `position` (None = start), and auto-plays per `play`. Emitted on a
    /// no-pipeline play or when leaving exclusive mode.
    ReplayRequest {
        track: TrackInfo,
        expected_gen: u32,
        position: Option<f64>,
        play: bool,
    },
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    VolumeSync(f64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ResumePolicy {
    Disabled,
    Auto,
    Explicit(f64),
}

struct LoadRequest {
    buffer: RamBuffer,
    load_gen: u32,
    seq: u32,
    track_id: String,
    resume_policy: ResumePolicy,
    load_start: std::time::Instant,
    cached: bool,
    format: String,
}

enum LoadStep {
    Handled,
    Miss,
}

struct LoadContext {
    load_gen: u32,
    event_seq: u32,
    load_start: std::time::Instant,
    resume_policy: ResumePolicy,
    auto_play: bool,
    cmd_tx: mpsc::Sender<PlayerCommand>,
    format: String,
    /// Cancelled when a newer load supersedes this one or on stop.
    cancel_token: CancellationToken,
}

impl LoadContext {
    fn is_stale(&self) -> bool {
        LOAD_SEQ.load(Relaxed) != self.load_gen
    }

    fn publish_load(&self, buffer: RamBuffer, cached: bool, track_id: String) {
        let _ = self.cmd_tx.send(PlayerCommand::Load {
            request: LoadRequest {
                buffer,
                load_gen: self.load_gen,
                seq: self.event_seq,
                track_id,
                resume_policy: self.resume_policy,
                load_start: self.load_start,
                cached,
                format: self.format.clone(),
            },
            auto_play: self.auto_play,
        });
    }

    fn fail_load(&self, error: String, code: MediaErrorCode) {
        let _ = self.cmd_tx.send(PlayerCommand::LoadFailed {
            error,
            code,
            seq: self.event_seq,
            load_gen: self.load_gen,
        });
    }
}

/// Sends `LoadSettled{generation}` on Drop, so the thread's in-flight marker is
/// cleared on every task exit (completion, return, panic, cancellation) - a
/// panicked/aborted load can't leave a play deferring forever. Gen-matched on
/// the thread side, so a stale settle is a no-op.
struct LoadSettleGuard {
    cmd_tx: mpsc::Sender<PlayerCommand>,
    generation: u32,
}

impl Drop for LoadSettleGuard {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(PlayerCommand::LoadSettled {
            generation: self.generation,
        });
    }
}

enum PlayerCommand {
    Load {
        request: LoadRequest,
        auto_play: bool,
    },
    /// A load for `generation` has begun; the thread marks it in-flight.
    LoadStarted {
        generation: u32,
    },
    /// A load for `generation` ended (delivered/failed/dropped); the thread
    /// clears its in-flight marker and any play deferred on it.
    LoadSettled {
        generation: u32,
    },
    Play,
    /// A same-track re-assert from `Player::load`'s idempotent branch. Resumes
    /// if the track was playing when the pause-retain stop happened
    /// (`resume_on_reassert`) or if the load carried the user's play-intent
    /// (`want_play`); a user-paused re-assert with no intent stays paused.
    ReassertResume {
        want_play: bool,
    },
    Pause,
    Stop(u32),
    Seek(f64),
    SetVolume(f64),
    GetAudioDevices(Option<String>),
    SetAudioDevice {
        id: String,
        mode: OutputMode,
    },
    /// A load failed before reaching the pipeline: report the error, then settle
    /// the SDK's `load()` with `Duration(0.0, seq)` - its `mediaduration` await
    /// has no timeout, so a mediaerror alone hangs it forever. Dropped when
    /// `load_gen` is superseded (a stale failure must not settle a newer load).
    LoadFailed {
        error: String,
        code: MediaErrorCode,
        seq: u32,
        load_gen: u32,
    },
    EmitMaxConnections,
    #[cfg(target_os = "windows")]
    SetVolumeSync(bool),
}

pub struct Player {
    cmd_tx: mpsc::Sender<PlayerCommand>,
    rt_handle: tokio::runtime::Handle,
    load_handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Current download's cancellation token; cancelled on a new load or stop.
    download_cancel: std::sync::Mutex<Option<CancellationToken>>,
    /// The track currently committed in the pipeline, as `(canonical_id, format)`,
    /// or `None` when nothing is loaded. The player thread sets it in `handle_load`
    /// (commit time, alongside `current_track_id`) and clears it on every track-end
    /// path; `load` reads it on the IPC thread to resume a same-track re-assert
    /// instead of rebuilding. It tracks the *committed* track, not the *requested*
    /// one, so a duplicate load racing a still-in-flight load of a different track
    /// can't falsely match and resume a stale pipeline. stop() is pause-retain, so
    /// this survives a stop and the following play() resumes it in place.
    committed_track: std::sync::Arc<std::sync::Mutex<Option<(String, String)>>>,
}

pub(crate) use crate::util::fmt::{format_bytes, format_ms, short_id};

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

pub(crate) fn canonical_track_id(url: &str) -> String {
    url.split('?').next().unwrap_or(url).to_string()
}

/// True when a `load` targets the already-committed track: same canonical id
/// (query stripped, since each load re-signs the CDN URL) and format. Re-loading
/// would rebuild an identical pipeline, so the caller resumes instead. Both ids
/// are canonical (caller strips the URL; `committed.0` is set at commit time).
fn is_same_active_track(
    committed: Option<&(String, String)>,
    canonical_url_id: &str,
    format: &str,
) -> bool {
    committed.is_some_and(|(id, fmt)| fmt == format && id == canonical_url_id)
}

fn print_track_banner(format: &str) {
    let Ok(lock) = CURRENT_METADATA.lock() else {
        crate::vprintln!("[PLAYER] CURRENT_METADATA lock poisoned, skipping banner");
        return;
    };
    let format_upper = format.to_uppercase();
    let (title, artist, quality) = match lock.as_ref() {
        Some(m) => (
            if m.title.is_empty() {
                "Unknown"
            } else {
                m.title.as_str()
            },
            if m.artist.is_empty() {
                "Unknown"
            } else {
                m.artist.as_str()
            },
            if m.quality.is_empty() {
                format_upper.as_str()
            } else {
                m.quality.as_str()
            },
        ),
        None => ("Unknown", "Unknown", format_upper.as_str()),
    };
    crate::vprintln!("══════════════════════════════════════════");
    crate::vprintln!("  {} - {}", title, artist);
    crate::vprintln!("  Quality: {} | Format: {}", quality, format);
    crate::vprintln!("══════════════════════════════════════════");
}

/// Does this look like a media container we could decode? Rejects a cache entry
/// whose key no longer fits it. Format-agnostic on purpose: the `format` string
/// comes from TIDAL's SDK and its vocabulary is not ours to enumerate.
fn looks_like_media(data: &[u8]) -> bool {
    if data.len() < 12 {
        return false;
    }
    data.starts_with(b"fLaC")
        || &data[4..8] == b"ftyp" // ISO BMFF: mp4 / m4a
        || data.starts_with(b"OggS")
        || data.starts_with(b"ID3")
        || looks_like_mpeg_frame(data)
}

/// Does `data` open on a plausible MPEG-audio or ADTS-AAC frame header? The sync
/// word alone is far weaker than the four-byte magics beside it (11 bits accept 1
/// random byte pair in 2048), so reserved and invalid fields are rejected too, and
/// ADTS must have a successor frame where its own length field says. Raw ADTS does
/// reach here: symphonia is built with `aac` and the decoder probes with no hint.
fn looks_like_mpeg_frame(data: &[u8]) -> bool {
    if data.len() < 7 || data[0] != 0xFF {
        return false;
    }

    // Layer 00 with the 12th sync bit is ADTS, a value MPEG audio reserves, so the
    // two families cannot be confused.
    if data[1] & 0xF0 == 0xF0 && data[1] & 0x06 == 0 {
        // 13..15 are reserved rates; channel config 0 defers to an
        // AudioSpecificConfig an ADTS stream never carries.
        let sampling = (data[2] >> 2) & 0x0F;
        let channels = ((data[2] & 0x01) << 2) | (data[3] >> 6);
        if sampling > 12 || channels == 0 {
            return false;
        }
        // A frame cannot be shorter than the header it starts with.
        let frame_len = (((data[3] & 0x03) as usize) << 11)
            | ((data[4] as usize) << 3)
            | ((data[5] >> 5) as usize);
        if frame_len < 7 {
            return false;
        }
        return match data.get(frame_len..frame_len + 2) {
            Some(next) => next[0] == 0xFF && next[1] & 0xF0 == 0xF0,
            // The successor lies past what we were given; the header alone stands.
            None => true,
        };
    }

    if data[1] & 0xE0 != 0xE0 {
        return false;
    }
    // Version 01, layer 00 and sampling 11 are reserved; bitrate 1111 is invalid and
    // 0000 is the free format nothing here emits. Finding the next frame would need
    // the bitrate tables, so this stops at the header - what slips through is caught
    // by the decoder, which retires the entry.
    let version = (data[1] >> 3) & 0x03;
    let layer = (data[1] >> 1) & 0x03;
    let bitrate = (data[2] >> 4) & 0x0F;
    let sampling = (data[2] >> 2) & 0x03;
    version != 0x01 && layer != 0x00 && bitrate != 0x00 && bitrate != 0x0F && sampling != 0x03
}

/// Why a cache entry could not be turned into playable bytes. The distinction is
/// load-bearing: only `Corrupt` condemns the stored file.
enum CacheReadError {
    /// The file is gone from disk, so the index row is an orphan.
    Orphaned,
    /// Could not read the file, or could not use the key. Neither says anything about
    /// the stored ciphertext, so the entry must survive: evicting would discard a good
    /// file the next attempt would have to download again.
    Unreadable(String),
    /// Decrypted, but not into a container we recognise, so the stored ciphertext no
    /// longer matches this key.
    Corrupt(String),
}

/// Read a cache entry and hand back playable bytes. Blocking and CPU-bound (a whole
/// file read plus an AES pass over every byte), so callers run it off the runtime.
fn read_cache_entry(path: &std::path::Path, key: &str) -> Result<Vec<u8>, CacheReadError> {
    let mut data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CacheReadError::Orphaned);
        }
        Err(e) => return Err(CacheReadError::Unreadable(format!("read failed: {e}"))),
    };

    // Key-side only: `new` unwraps the key ID and `decrypt_in_place` can fail solely
    // while building the cipher from it, so neither outcome depends on the disk bytes.
    if !key.is_empty()
        && let Err(e) = crate::audio::decrypt::FlacDecryptor::new(key)
            .and_then(|d| d.decrypt_in_place(&mut data, 0))
    {
        return Err(CacheReadError::Unreadable(format!("unusable key: {e}")));
    }

    // AES-CTR is unauthenticated, so a stale key decrypts to noise and still returns
    // Ok. This must run before `touch`, which would refresh the LRU position and keep
    // a bad entry from ever evicting.
    if !looks_like_media(&data) {
        return Err(CacheReadError::Corrupt(
            "not a known container after decrypt".to_string(),
        ));
    }

    Ok(data)
}

/// `key` is the track's wrapped content key: cache files hold TIDAL's
/// ciphertext, so a hit is only playable once decrypted. An empty key means the
/// stream was never encrypted, matching the download path's own check.
async fn try_cache_hit(ctx: &LoadContext, track_id: &str, key: &str) -> LoadStep {
    let cache_t0 = std::time::Instant::now();

    // Short lock: index lookup only (no disk I/O)
    let path = {
        let Ok(cache) = AUDIO_CACHE.lock() else {
            crate::vprintln!("[CACHE]  Lock poisoned, skipping cache lookup");
            return LoadStep::Miss;
        };
        cache.lookup_path(track_id)
    };

    let Some(path) = path else {
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
                cache.remove_index_entry(track_id);
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
            crate::vprintln!(
                "[CACHE]  Dropping unusable entry {}: {reason}",
                short_id(track_id, 40)
            );
            if let Ok(mut cache) = AUDIO_CACHE.lock() {
                cache.drop_entry(track_id);
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
    ctx.publish_load(buffer, true, track_id.to_string());
    LoadStep::Handled
}

async fn try_preload_hit(ctx: &LoadContext, track: &TrackInfo, track_id: &str) -> LoadStep {
    let preload_t0 = std::time::Instant::now();
    if let Some(preloaded) = preload::take_preloaded_if_match(track).await {
        let preload_ms = preload_t0.elapsed().as_secs_f64() * 1000.0;
        if ctx.is_stale() {
            crate::vprintln!(
                "[LOAD #{}] stale after preload check, dropping",
                ctx.load_gen
            );
            return LoadStep::Handled;
        }
        crate::vprintln!(
            "[PRELOAD] Hit: {} | check: {} | total: {}",
            format_bytes(preloaded.data.len() as u64),
            format_ms(preload_ms),
            format_ms(ctx.load_start.elapsed().as_secs_f64() * 1000.0)
        );
        let buffer = RamBuffer::from_complete_with_ciphertext(preloaded.data, preloaded.ciphertext);
        ctx.publish_load(buffer, false, track_id.to_string());
        return LoadStep::Handled;
    }

    let preload_ms = preload_t0.elapsed().as_secs_f64() * 1000.0;
    crate::vprintln!("[PRELOAD] Miss | check: {}", format_ms(preload_ms));
    LoadStep::Miss
}

async fn start_stream_load(ctx: &LoadContext, url: &str, key: &str, track_id: &str) {
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
                if status.as_u16() == 429 {
                    let _ = ctx.cmd_tx.send(PlayerCommand::EmitMaxConnections);
                } else {
                    ctx.fail_load(format!("HTTP {}", status), MediaErrorCode::NoSuchFile);
                }
                return;
            }
            r
        }
        Err(e) => {
            crate::vprintln!("[ERROR]  Request failed: {}", e);
            ctx.fail_load(format!("request failed: {e}"), MediaErrorCode::NoSuchFile);
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
        match preload::fetch_and_decrypt(url, key).await {
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
                ctx.publish_load(buffer, false, track_id.to_string());
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

    let (buffer, writer) = RamBuffer::new(total_len);

    preload::start_download(
        resp,
        url.to_string(),
        key.to_string(),
        writer,
        ctx.cancel_token.clone(),
    );

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
        if buffer.written() >= PRE_BUFFER_TARGET {
            break;
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
    ctx.publish_load(buffer, false, track_id.to_string());
}

impl Player {
    pub fn new<F>(callback: F, rt_handle: tokio::runtime::Handle) -> anyhow::Result<Self>
    where
        F: Fn(PlayerEvent) + Send + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel::<PlayerCommand>();

        #[cfg(target_os = "windows")]
        let volume_sync_enabled =
            crate::state::db().call_settings(crate::settings::load_volume_sync);
        #[cfg(not(target_os = "windows"))]
        let volume_sync_enabled = true;

        let committed_track = std::sync::Arc::new(std::sync::Mutex::new(None::<(String, String)>));
        let thread_committed = committed_track.clone();
        std::thread::spawn(move || {
            if let Some(mut pt) =
                thread::PlayerThread::new(cmd_rx, callback, volume_sync_enabled, thread_committed)
            {
                pt.run();
            }
        });

        Ok(Self {
            cmd_tx,
            rt_handle,
            load_handle: std::sync::Mutex::new(None),
            download_cancel: std::sync::Mutex::new(None),
            committed_track,
        })
    }

    fn load_with_policy(
        &self,
        url: String,
        format: String,
        key: String,
        resume_policy: ResumePolicy,
        auto_play: bool,
    ) -> anyhow::Result<()> {
        let load_gen = LOAD_SEQ.fetch_add(1, Relaxed) + 1;
        let _ = self.cmd_tx.send(PlayerCommand::LoadStarted {
            generation: load_gen,
        });
        let event_seq = EVENT_SEQ.fetch_add(1, Relaxed) + 1;
        crate::vprintln!("[LOAD #{load_gen}] start");
        print_track_banner(&format);

        if let Some(prev) = self
            .load_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            prev.abort();
        }

        // Cancel the previous track's download so a skip doesn't leak it streaming a
        // full file into RAM (start_download races this token).
        let cancel_token = CancellationToken::new();
        if let Some(prev) = self
            .download_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(cancel_token.clone())
        {
            prev.cancel();
        }

        // Reset governor buffer progress so the new download isn't throttled
        // by stale counters from the previous track.
        GOVERNOR.reset_buffer_progress();

        {
            let mut lock = CURRENT_TRACK.lock().unwrap_or_else(|e| e.into_inner());
            *lock = Some(TrackInfo {
                url: url.clone(),
                format: format.clone(),
                key: key.clone(),
            });
        }

        let ctx = LoadContext {
            load_gen,
            event_seq,
            load_start: std::time::Instant::now(),
            resume_policy,
            auto_play,
            cmd_tx: self.cmd_tx.clone(),
            format: format.clone(),
            cancel_token,
        };

        let handle = self.rt_handle.spawn(async move {
            let _settle = LoadSettleGuard {
                cmd_tx: ctx.cmd_tx.clone(),
                generation: ctx.load_gen,
            };
            let track = TrackInfo {
                url: url.clone(),
                format: format.clone(),
                key: key.clone(),
            };
            let track_id = canonical_track_id(&url);

            if let LoadStep::Handled = try_cache_hit(&ctx, &track_id, &key).await {
                return;
            }
            if let LoadStep::Handled = try_preload_hit(&ctx, &track, &track_id).await {
                return;
            }
            start_stream_load(&ctx, &url, &key, &track_id).await;
        });

        *self.load_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);

        Ok(())
    }

    pub fn load(
        &self,
        url: String,
        format: String,
        key: String,
        restart: bool,
        want_play: bool,
    ) -> anyhow::Result<()> {
        // Idempotent reconcile: when the SDK re-loads the track currently COMMITTED in
        // the pipeline, either restart it (a fresh play instance) or resume it in place
        // (a keep-position re-assert), without a full rebuild. The comparison is against
        // the committed track (set by the player thread in handle_load), never the
        // requested track, so a duplicate load racing a still-in-flight load of a
        // different track can't falsely match and touch a stale pipeline. stop() is
        // pause-retain, so the committed track survives. A genuine track change
        // (different canonical id/format) falls through to a full load.
        let canonical_url_id = canonical_track_id(&url);
        let same_committed = {
            let committed = self
                .committed_track
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            is_same_active_track(committed.as_ref(), &canonical_url_id, &format)
        };
        if same_committed {
            if restart {
                // Fresh play instance (TIDAL minted a new referenceId, threaded here as
                // `restart`): per the SDK's load contract (load = start at 0), restart from 0
                // via a fresh cached load (cache hit, no re-download), auto-playing only if
                // want_play. Reporting position 0 also stops Redux's position-reconcile seek
                // burst that a kept position would otherwise provoke.
                crate::vprintln!(
                    "[LOAD]   same track, fresh play instance -> restart at 0 (want_play={}): {}",
                    want_play,
                    short_id(&canonical_url_id, 60)
                );
                return self.load_with_policy(url, format, key, ResumePolicy::Disabled, want_play);
            }
            // Same-track re-assert keeping the play instance (a quality-swap re-load):
            // resumes if PLAYING pre-stop (resume_on_reassert), or if want_play is set --
            // click-to-play on a restored paused track is stop+load(same) with NO
            // player.play (SDK-verified), so want_play is the only resume signal then.
            crate::vprintln!(
                "[LOAD]   idempotent reload skipped (same committed track, want_play={}): {}",
                want_play,
                short_id(&canonical_url_id, 60)
            );
            let _ = self.send_cmd(PlayerCommand::ReassertResume { want_play });
            return Ok(());
        }
        // Different track (genuine select / queue advance): fresh load. `want_play` folds
        // the SELECT's play-intent into the load so no separate player.play arrives while
        // the OLD track is still committed (decide_play would Resume it -> audible bleed).
        self.load_with_policy(url, format, key, ResumePolicy::Disabled, want_play)
    }

    pub fn load_and_play(&self, url: String, format: String, key: String) -> anyhow::Result<()> {
        self.load_with_policy(url, format, key, ResumePolicy::Disabled, true)
    }

    /// Load a DASH stream by fetching init + media segments and concatenating them.
    pub fn load_dash(
        &self,
        init_url: String,
        segment_urls: Vec<String>,
        format: String,
    ) -> anyhow::Result<()> {
        let load_gen = LOAD_SEQ.fetch_add(1, Relaxed) + 1;
        let _ = self.cmd_tx.send(PlayerCommand::LoadStarted {
            generation: load_gen,
        });
        let event_seq = EVENT_SEQ.fetch_add(1, Relaxed) + 1;
        crate::vprintln!(
            "[DASH-LOAD #{load_gen}] start - {} segments",
            segment_urls.len()
        );
        print_track_banner(&format);

        if let Some(prev) = self
            .load_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            prev.abort();
        }
        GOVERNOR.reset_buffer_progress();

        // DASH isn't replayable via load_and_play (segmented), so clear the
        // retained source: a post-DASH play re-arms nothing, not a stale track.
        *CURRENT_TRACK.lock().unwrap_or_else(|e| e.into_inner()) = None;

        let cmd_tx = self.cmd_tx.clone();
        let handle = self.rt_handle.spawn(async move {
            let _settle = LoadSettleGuard {
                cmd_tx: cmd_tx.clone(),
                generation: load_gen,
            };
            let load_start = std::time::Instant::now();
            let is_stale = || LOAD_SEQ.load(Relaxed) != load_gen;

            crate::vprintln!("[DASH-LOAD #{load_gen}] fetching init segment...");
            let init_data = match HTTP_CLIENT_PLAYBACK.get(&init_url).send().await {
                Ok(r) if r.status().is_success() => match r.bytes().await {
                    Ok(b) => b.to_vec(),
                    Err(e) => {
                        crate::vprintln!("[ERROR]  DASH init segment read failed: {e}");
                        let _ = cmd_tx.send(PlayerCommand::LoadFailed {
                            error: format!("DASH init segment: {e}"),
                            code: MediaErrorCode::NoSuchFile,
                            seq: event_seq,
                            load_gen,
                        });
                        return;
                    }
                },
                Ok(r) => {
                    crate::vprintln!("[ERROR]  DASH init segment HTTP {}", r.status());
                    let _ = cmd_tx.send(PlayerCommand::LoadFailed {
                        error: format!("DASH init HTTP {}", r.status()),
                        code: MediaErrorCode::NoSuchFile,
                        seq: event_seq,
                        load_gen,
                    });
                    return;
                }
                Err(e) => {
                    crate::vprintln!("[ERROR]  DASH init segment request failed: {e}");
                    let _ = cmd_tx.send(PlayerCommand::LoadFailed {
                        error: format!("DASH init request: {e}"),
                        code: MediaErrorCode::NoSuchFile,
                        seq: event_seq,
                        load_gen,
                    });
                    return;
                }
            };

            if is_stale() {
                crate::vprintln!("[DASH-LOAD #{load_gen}] stale after init, dropping");
                return;
            }

            crate::vprintln!(
                "[DASH-LOAD #{load_gen}] init: {} | {:.0}ms",
                format_bytes(init_data.len() as u64),
                load_start.elapsed().as_secs_f64() * 1000.0
            );

            let segment_count = segment_urls.len();

            // Fetch segments with bounded concurrency, appending each to the
            // output buffer as it arrives and dropping it immediately. This caps
            // both peak RAM (no transient second copy of the whole file) and the
            // number of simultaneous CDN connections, unlike a join_all that
            // fires every segment request at once. `buffered` preserves order.
            const DASH_MAX_CONCURRENT: usize = 6;

            let mut mp4_data = init_data;
            let mut segments = stream::iter(segment_urls.into_iter().enumerate())
                .map(|(i, url)| async move {
                    match HTTP_CLIENT_PLAYBACK.get(&url).send().await {
                        Ok(r) if r.status().is_success() => r
                            .bytes()
                            .await
                            .map_err(|e| format!("DASH segment {i} read: {e}")),
                        Ok(r) => Err(format!("DASH segment {i} HTTP {}", r.status())),
                        Err(e) => Err(format!("DASH segment {i} request: {e}")),
                    }
                })
                .buffered(DASH_MAX_CONCURRENT);

            while let Some(result) = segments.next().await {
                if is_stale() {
                    crate::vprintln!("[DASH-LOAD #{load_gen}] stale mid-fetch, dropping");
                    return;
                }
                match result {
                    Ok(data) => mp4_data.extend_from_slice(&data),
                    Err(msg) => {
                        crate::vprintln!("[ERROR]  {msg}");
                        let _ = cmd_tx.send(PlayerCommand::LoadFailed {
                            error: msg,
                            code: MediaErrorCode::NoSuchFile,
                            seq: event_seq,
                            load_gen,
                        });
                        return;
                    }
                }
            }

            let total_ms = load_start.elapsed().as_secs_f64() * 1000.0;
            crate::vprintln!(
                "[DASH-LOAD #{load_gen}] done: {} segments, {} total in {:.0}ms",
                segment_count,
                format_bytes(mp4_data.len() as u64),
                total_ms
            );

            let buffer = RamBuffer::from_complete(mp4_data);
            let track_id = format!("dash-{load_gen}");
            let _ = cmd_tx.send(PlayerCommand::Load {
                request: LoadRequest {
                    buffer,
                    load_gen,
                    seq: event_seq,
                    track_id,
                    resume_policy: ResumePolicy::Disabled,
                    load_start,
                    cached: false,
                    format,
                },
                auto_play: true,
            });
        });

        *self.load_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        Ok(())
    }

    /// An explicit target seeks there (when past the resume floor); otherwise
    /// fall back to the stored resume position for the track.
    fn resume_policy_for(target_time: Option<f64>) -> ResumePolicy {
        match target_time {
            Some(t) if t.is_finite() && t > resume::RESUME_MIN_SECONDS => ResumePolicy::Explicit(t),
            _ => ResumePolicy::Auto,
        }
    }

    pub fn recover(
        &self,
        url: String,
        format: String,
        key: String,
        target_time: Option<f64>,
    ) -> anyhow::Result<()> {
        self.load_with_policy(
            url,
            format,
            key,
            Self::resume_policy_for(target_time),
            false,
        )
    }

    /// Re-arm the shared pipeline from a `ReplayRequest`: `Some(position)` seeks
    /// there (past the resume floor; below it starts from the beginning), `None`
    /// restarts; `play` auto-plays. Single entry point for the ReplayRequest
    /// consumer, keeping the dispatch out of flush.rs.
    pub fn rearm(
        &self,
        url: String,
        format: String,
        key: String,
        position: Option<f64>,
        play: bool,
    ) -> anyhow::Result<()> {
        // Honor the caller position directly (live re-arm target, not a
        // resume-store candidate): resume_policy_for would floor sub-1s to Auto
        // and seek a stale persisted offset. Explicit applies the floor itself.
        let policy = match position {
            Some(t) => ResumePolicy::Explicit(t),
            None => ResumePolicy::Disabled,
        };
        self.load_with_policy(url, format, key, policy, play)
    }

    fn send_cmd(&self, cmd: PlayerCommand) -> anyhow::Result<()> {
        self.cmd_tx
            .send(cmd)
            .map_err(|_| anyhow::anyhow!("Player thread is dead"))
    }

    pub fn play(&self) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::Play)
    }

    pub fn pause(&self) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::Pause)
    }

    pub fn stop(&self) -> anyhow::Result<()> {
        // Pause-retain, not teardown: the in-flight load task and its download are
        // kept (the download rides its own download_cancel token, not LOAD_SEQ) so a
        // same-track re-assert resumes in place. The LOAD_SEQ bump invalidates any
        // still-pending auto-play Load/ReplayRequest (rejected by handle_load's
        // stale-gate and the flush.rs guard) so playback can't start after the stop;
        // it doesn't truncate the retained download, and a steady-state re-assert
        // resumes via Player::load, which bypasses both gates.
        LOAD_SEQ.fetch_add(1, Relaxed);
        let event_seq = EVENT_SEQ.fetch_add(1, Relaxed) + 1;
        self.send_cmd(PlayerCommand::Stop(event_seq))
    }

    pub fn seek(&self, time: f64) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::Seek(time))
    }

    pub fn set_volume(&self, volume: f64) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::SetVolume(volume))
    }

    #[cfg(target_os = "windows")]
    pub fn set_volume_sync(&self, enabled: bool) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::SetVolumeSync(enabled))
    }

    pub fn get_audio_devices(&self, req_id: Option<String>) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::GetAudioDevices(req_id))
    }

    pub fn set_audio_device(&self, device_id: String, mode: OutputMode) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::SetAudioDevice {
            id: device_id,
            mode,
        })
    }
}

#[cfg(test)]
mod reconcile_tests {
    use super::is_same_active_track;

    fn committed(canonical_id: &str, format: &str) -> (String, String) {
        (canonical_id.to_string(), format.to_string())
    }

    #[test]
    fn same_canonical_id_and_format_is_idempotent() {
        // The caller strips the query before calling, so both ids are canonical.
        // (Production passes canonical_track_id("…?sig=2") == "…/abc".)
        let cur = committed("https://cdn/tracks/abc", "flac");
        assert!(is_same_active_track(
            Some(&cur),
            "https://cdn/tracks/abc",
            "flac"
        ));
    }

    #[test]
    fn different_format_rebuilds() {
        let cur = committed("https://cdn/tracks/abc", "flac");
        assert!(!is_same_active_track(
            Some(&cur),
            "https://cdn/tracks/abc",
            "aac"
        ));
    }

    #[test]
    fn different_track_rebuilds() {
        let cur = committed("https://cdn/tracks/abc", "flac");
        assert!(!is_same_active_track(
            Some(&cur),
            "https://cdn/tracks/xyz",
            "flac"
        ));
    }

    #[test]
    fn no_committed_track_rebuilds() {
        assert!(!is_same_active_track(
            None,
            "https://cdn/tracks/abc",
            "flac"
        ));
    }
}

#[cfg(test)]
mod media_error_code_tests {
    use super::MediaErrorCode;

    #[test]
    fn codes_match_the_sdk_map() {
        // Locked to nativePlayer.ts `mediaErrorCodeMap` (no_such_file -> NPO01,
        // unreadable_file -> NPO03): any other wire string reaches the SDK as
        // `errorCode: undefined`.
        assert_eq!(MediaErrorCode::NoSuchFile.as_str(), "no_such_file");
        assert_eq!(MediaErrorCode::UnreadableFile.as_str(), "unreadable_file");
    }
}

#[cfg(test)]
mod cache_entry_validation_tests {
    use super::{CacheReadError, looks_like_media, looks_like_mpeg_frame, read_cache_entry};

    fn flac() -> Vec<u8> {
        let mut v = b"fLaC\x00\x00\x00\x22".to_vec();
        v.extend_from_slice(&[0u8; 32]);
        v
    }

    #[test]
    fn known_containers_are_accepted() {
        assert!(looks_like_media(&flac()));

        let mut mp4 = vec![0, 0, 0, 0x20];
        mp4.extend_from_slice(b"ftypM4A ");
        mp4.extend_from_slice(&[0u8; 16]);
        assert!(looks_like_media(&mp4));

        let mut ogg = b"OggS".to_vec();
        ogg.extend_from_slice(&[0u8; 32]);
        assert!(looks_like_media(&ogg));
    }

    /// The case that matters: a key that no longer matches the stored ciphertext
    /// decrypts without error and yields noise. XOR-ing a real header stands in
    /// for that - AES-CTR is exactly an XOR against a keystream.
    #[test]
    fn bytes_decrypted_with_the_wrong_key_are_rejected() {
        let mut noise = flac();
        for (i, b) in noise.iter_mut().enumerate() {
            *b ^= 0x5A_u8.wrapping_add(i as u8);
        }
        assert!(!looks_like_media(&noise));
    }

    #[test]
    fn a_truncated_entry_is_rejected() {
        assert!(!looks_like_media(b""));
        assert!(!looks_like_media(b"fLaC"));
    }

    /// A 7-byte ADTS header: MPEG-4 AAC-LC, 44.1 kHz, stereo, `frame_len` bytes long.
    fn adts_header(frame_len: usize) -> [u8; 7] {
        [
            0xFF,
            0xF1,                                    // 12-bit sync, MPEG-4, layer 00, no CRC
            0x50,                                    // AAC-LC, sampling index 4
            0x80 | ((frame_len >> 11) & 0x03) as u8, // channel config 2 + length high bits
            ((frame_len >> 3) & 0xFF) as u8,
            (((frame_len & 0x07) << 5) as u8) | 0x1F,
            0xFC,
        ]
    }

    fn padded(head: &[u8], len: usize) -> Vec<u8> {
        let mut v = head.to_vec();
        v.resize(len, 0);
        v
    }

    #[test]
    fn a_real_adts_frame_is_accepted_when_its_successor_is_there() {
        let mut data = padded(&adts_header(32), 32);
        data.extend_from_slice(&adts_header(32));
        assert!(looks_like_mpeg_frame(&data));
        assert!(looks_like_media(&data));
    }

    /// Noise can pass the field checks; it will not also place a sync word exactly
    /// where the length field says.
    #[test]
    fn an_adts_frame_with_nothing_at_its_successor_is_rejected() {
        assert!(!looks_like_mpeg_frame(&padded(&adts_header(32), 64)));
    }

    #[test]
    fn an_adts_header_stands_alone_when_the_frame_runs_past_what_we_have() {
        assert!(looks_like_mpeg_frame(&padded(&adts_header(4096), 64)));
    }

    #[test]
    fn adts_headers_with_reserved_fields_are_rejected() {
        let mut reserved_rate = adts_header(32);
        reserved_rate[2] = 0x74; // sampling index 13
        assert!(!looks_like_mpeg_frame(&padded(&reserved_rate, 64)));

        let mut no_channels = adts_header(32);
        no_channels[3] = 0x00; // channel configuration 0
        assert!(!looks_like_mpeg_frame(&padded(&no_channels, 64)));
    }

    #[test]
    fn a_real_mp3_frame_is_accepted() {
        // MPEG-1 Layer III, 128 kbps, 44.1 kHz.
        assert!(looks_like_mpeg_frame(&padded(
            &[0xFF, 0xFB, 0x90, 0xC0],
            32
        )));
    }

    /// The whole point of the rewrite: a bare sync word used to be enough, and 1 random
    /// byte pair in 2048 has one.
    #[test]
    fn a_bare_sync_word_is_no_longer_enough() {
        assert!(!looks_like_mpeg_frame(&padded(
            &[0xFF, 0xE0, 0x90, 0xC0],
            32
        )));
    }

    #[test]
    fn mpeg_headers_with_reserved_or_invalid_fields_are_rejected() {
        // Version 01 is reserved.
        assert!(!looks_like_mpeg_frame(&padded(
            &[0xFF, 0xEB, 0x90, 0xC0],
            32
        )));
        // Bitrate index 1111 is invalid, 0000 is the free format.
        assert!(!looks_like_mpeg_frame(&padded(
            &[0xFF, 0xFB, 0xF0, 0xC0],
            32
        )));
        assert!(!looks_like_mpeg_frame(&padded(
            &[0xFF, 0xFB, 0x00, 0xC0],
            32
        )));
        // Sampling index 11 is reserved.
        assert!(!looks_like_mpeg_frame(&padded(
            &[0xFF, 0xFB, 0x9C, 0xC0],
            32
        )));
    }

    fn entry(dir: &std::path::Path, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join("entry");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// The distinction the whole enum exists for: key-side failures must not condemn
    /// a file that may be perfectly good.
    #[test]
    fn an_unusable_key_keeps_the_entry_instead_of_condemning_it() {
        let tmp = tempfile::tempdir().unwrap();
        let path = entry(tmp.path(), &flac());
        assert!(matches!(
            read_cache_entry(&path, "not a key id"),
            Err(CacheReadError::Unreadable(_))
        ));
    }

    #[test]
    fn a_missing_file_is_reported_as_an_orphaned_row() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            read_cache_entry(&tmp.path().join("gone"), ""),
            Err(CacheReadError::Orphaned)
        ));
    }

    #[test]
    fn bytes_that_are_not_a_container_condemn_the_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = entry(tmp.path(), &[0x11; 64]);
        assert!(matches!(
            read_cache_entry(&path, ""),
            Err(CacheReadError::Corrupt(_))
        ));
    }

    #[test]
    fn an_unencrypted_container_reads_back_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let path = entry(tmp.path(), &flac());
        assert_eq!(read_cache_entry(&path, "").ok(), Some(flac()));
    }
}
