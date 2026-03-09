pub(crate) mod buffer;
pub(crate) mod cache;
pub(crate) mod ipc;
mod resume;
mod thread;
#[cfg(target_os = "windows")]
pub(crate) mod wasapi;

use crate::preload;
use crate::state::{
    AUDIO_CACHE, CURRENT_METADATA, CURRENT_TRACK, GOVERNOR, HTTP_CLIENT_PLAYBACK, TrackInfo,
};
use buffer::RamBuffer;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::mpsc;

pub(crate) static LOAD_SEQ: AtomicU32 = AtomicU32::new(0);
static EVENT_SEQ: AtomicU32 = AtomicU32::new(0);
#[cfg(target_os = "windows")]
static EXCLUSIVE_STREAM_SEQ: AtomicU32 = AtomicU32::new(0);

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
    Locked,
    Disconnected,
    Unknown,
}

impl DeviceErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "devicenotfound",
            Self::FormatNotSupported => "deviceformatnotsupported",
            Self::ExclusiveModeNotAllowed => "deviceexclusivemodenotallowed",
            Self::Locked => "devicelocked",
            Self::Disconnected => "devicedisconnected",
            Self::Unknown => "deviceunknownerror",
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
    },
    Version(&'static str),
    MediaError {
        error: String,
        code: &'static str,
    },
    MaxConnectionsReached,
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
}

enum PlayerCommand {
    Load {
        request: LoadRequest,
        auto_play: bool,
    },
    Play,
    Pause,
    Stop(u32),
    Seek(f64),
    SetVolume(f64),
    GetAudioDevices(Option<String>),
    SetAudioDevice {
        id: String,
        exclusive: bool,
    },
    EmitMediaError {
        error: String,
        code: &'static str,
    },
    EmitMaxConnections,
}

pub struct Player {
    cmd_tx: mpsc::Sender<PlayerCommand>,
    rt_handle: tokio::runtime::Handle,
    load_handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

fn format_ms(ms: f64) -> String {
    if ms < 1.0 {
        format!("{:.0}µs", ms * 1000.0)
    } else {
        format!("{:.0}ms", ms)
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

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
            info.push_str(&format!(" | {label}={val}"));
        }
    }
    crate::vprintln!("{prefix} {info}");
}

fn canonical_track_id(url: &str) -> String {
    url.split('?').next().unwrap_or(url).to_string()
}

fn print_track_banner(format: &str) {
    let lock = CURRENT_METADATA.lock().unwrap();
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
    crate::vprintln!("  {} — {}", title, artist);
    crate::vprintln!("  Quality: {} | Format: {}", quality, format);
    crate::vprintln!("══════════════════════════════════════════");
}

impl Player {
    pub fn new<F>(callback: F, rt_handle: tokio::runtime::Handle) -> anyhow::Result<Self>
    where
        F: Fn(PlayerEvent) + Send + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel::<PlayerCommand>();

        std::thread::spawn(move || {
            if let Some(mut pt) = thread::PlayerThread::new(cmd_rx, callback) {
                pt.run();
            }
        });

        Ok(Self {
            cmd_tx,
            rt_handle,
            load_handle: std::sync::Mutex::new(None),
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
        let event_seq = EVENT_SEQ.fetch_add(1, Relaxed) + 1;
        crate::vprintln!("[LOAD #{load_gen}] start");
        print_track_banner(&format);

        if let Some(prev) = self.load_handle.lock().unwrap().take() {
            prev.abort();
        }

        // Reset governor buffer progress so the new download isn't throttled
        // by stale counters from the previous track.
        GOVERNOR.reset_buffer_progress();

        {
            let mut lock = CURRENT_TRACK.lock().unwrap();
            *lock = Some(TrackInfo {
                url: url.clone(),
                format: format.clone(),
                key: key.clone(),
            });
        }

        let cmd_tx = self.cmd_tx.clone();
        let handle = self.rt_handle.spawn(async move {
            let load_start = std::time::Instant::now();
            let is_stale = || LOAD_SEQ.load(Relaxed) != load_gen;
            let send_load = |buffer: RamBuffer, cached: bool, track_id: String| {
                let _ = cmd_tx.send(PlayerCommand::Load {
                    request: LoadRequest {
                        buffer,
                        load_gen,
                        seq: event_seq,
                        track_id,
                        resume_policy,
                        load_start,
                        cached,
                    },
                    auto_play,
                });
            };
            let track = TrackInfo {
                url: url.clone(),
                format: format.clone(),
                key: key.clone(),
            };
            let track_id = canonical_track_id(&url);

            {
                let cache_t0 = std::time::Instant::now();
                let cache = AUDIO_CACHE.lock().unwrap();
                if let Some(data) = cache.load(&track_id) {
                    let cache_ms = cache_t0.elapsed().as_secs_f64() * 1000.0;
                    if is_stale() {
                        return;
                    }
                    crate::vprintln!(
                        "[CACHE]  Hit: {} | {} | read: {} | total: {}",
                        track_id.chars().take(40).collect::<String>(),
                        format_bytes(data.len() as u64),
                        format_ms(cache_ms),
                        format_ms(load_start.elapsed().as_secs_f64() * 1000.0)
                    );
                    let buffer = RamBuffer::from_complete(data);
                    send_load(buffer, true, track_id);
                    return;
                }
                let cache_ms = cache_t0.elapsed().as_secs_f64() * 1000.0;
                crate::vprintln!(
                    "[CACHE]  Miss ({}) | lookup: {}",
                    track_id.chars().take(40).collect::<String>(),
                    format_ms(cache_ms)
                );
            }

            let preload_t0 = std::time::Instant::now();
            if let Some(preloaded) = preload::take_preloaded_if_match(&track).await {
                let preload_ms = preload_t0.elapsed().as_secs_f64() * 1000.0;
                if is_stale() {
                    crate::vprintln!("[LOAD #{load_gen}] stale after preload check, dropping");
                    return;
                }
                crate::vprintln!(
                    "[PRELOAD] Hit: {} | check: {} | total: {}",
                    format_bytes(preloaded.data.len() as u64),
                    format_ms(preload_ms),
                    format_ms(load_start.elapsed().as_secs_f64() * 1000.0)
                );
                let buffer = RamBuffer::from_complete(preloaded.data);
                send_load(buffer, false, track_id);
                return;
            }

            let preload_ms = preload_t0.elapsed().as_secs_f64() * 1000.0;
            crate::vprintln!("[PRELOAD] Miss | check: {}", format_ms(preload_ms));

            if is_stale() {
                crate::vprintln!("[LOAD #{load_gen}] stale before HTTP, dropping");
                return;
            }

            let req_start = std::time::Instant::now();
            let resp = match HTTP_CLIENT_PLAYBACK.get(&url).send().await {
                Ok(r) => {
                    if !r.status().is_success() {
                        let status = r.status();
                        eprintln!("[ERROR]  Upstream status: {}", status);
                        if status.as_u16() == 429 {
                            let _ = cmd_tx.send(PlayerCommand::EmitMaxConnections);
                        } else {
                            let _ = cmd_tx.send(PlayerCommand::EmitMediaError {
                                error: format!("HTTP {}", status),
                                code: "no_such_file",
                            });
                        }
                        return;
                    }
                    r
                }
                Err(e) => {
                    eprintln!("[ERROR]  Request failed: {}", e);
                    let _ = cmd_tx.send(PlayerCommand::EmitMediaError {
                        error: format!("request failed: {e}"),
                        code: "no_such_file",
                    });
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

            if is_stale() {
                crate::vprintln!("[LOAD #{load_gen}] stale after HTTP TTFB, dropping");
                return;
            }

            let total_len = resp.content_length().unwrap_or(0);
            if total_len == 0 {
                crate::vprintln!(
                    "[FETCH]  No Content-Length, full download... (TTFB: {})",
                    format_ms(ttfb_ms)
                );
                match preload::fetch_and_decrypt(&url, &key).await {
                    Ok(data) => {
                        if is_stale() {
                            crate::vprintln!(
                                "[LOAD #{load_gen}] stale after full download, dropping"
                            );
                            return;
                        }
                        crate::vprintln!(
                            "[FETCH]  Done ({} in {:.0}ms)",
                            format_bytes(data.len() as u64),
                            load_start.elapsed().as_secs_f64() * 1000.0
                        );
                        let buffer = RamBuffer::from_complete(data);
                        send_load(buffer, false, track_id);
                    }
                    Err(e) => {
                        eprintln!("[ERROR]  Fetch failed: {}", e);
                        let _ = cmd_tx.send(PlayerCommand::EmitMediaError {
                            error: format!("fetch failed: {e}"),
                            code: "no_such_file",
                        });
                    }
                }
                return;
            }

            crate::vprintln!(
                "[NET]    Size: {} (load #{load_gen})",
                format_bytes(total_len)
            );

            let (buffer, writer) = RamBuffer::new(total_len);

            preload::start_download(resp, url, key, writer);

            // Pre-buffer 64KB before handing to decoder
            const PRE_BUFFER_TARGET: u64 = 64 * 1024;
            const PRE_BUFFER_TIMEOUT_MS: u64 = 2000;

            let prebuf_start = std::time::Instant::now();
            let prebuf_deadline =
                prebuf_start + std::time::Duration::from_millis(PRE_BUFFER_TIMEOUT_MS);
            loop {
                if is_stale() {
                    crate::vprintln!("[LOAD #{load_gen}] stale during pre-buffer, dropping");
                    buffer.cancel();
                    return;
                }
                if buffer.written() >= PRE_BUFFER_TARGET {
                    break;
                }
                let remaining =
                    prebuf_deadline.saturating_duration_since(std::time::Instant::now());
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
            send_load(buffer, false, track_id);
        });

        *self.load_handle.lock().unwrap() = Some(handle);

        Ok(())
    }

    pub fn load(&self, url: String, format: String, key: String) -> anyhow::Result<()> {
        self.load_with_policy(url, format, key, ResumePolicy::Disabled, false)
    }

    pub fn load_and_play(&self, url: String, format: String, key: String) -> anyhow::Result<()> {
        self.load_with_policy(url, format, key, ResumePolicy::Disabled, true)
    }

    pub fn recover(
        &self,
        url: String,
        format: String,
        key: String,
        target_time: Option<f64>,
    ) -> anyhow::Result<()> {
        let policy = match target_time {
            Some(t) if t.is_finite() && t > resume::RESUME_MIN_SECONDS => ResumePolicy::Explicit(t),
            _ => ResumePolicy::Auto,
        };
        self.load_with_policy(url, format, key, policy, false)
    }

    pub fn play(&self) -> anyhow::Result<()> {
        self.cmd_tx
            .send(PlayerCommand::Play)
            .map_err(|_| anyhow::anyhow!("Player thread is dead"))
    }

    pub fn pause(&self) -> anyhow::Result<()> {
        self.cmd_tx
            .send(PlayerCommand::Pause)
            .map_err(|_| anyhow::anyhow!("Player thread is dead"))
    }

    pub fn stop(&self) -> anyhow::Result<()> {
        LOAD_SEQ.fetch_add(1, Relaxed);
        let event_seq = EVENT_SEQ.fetch_add(1, Relaxed) + 1;
        crate::vprintln!(
            "[STOP]   (invalidated load seq: {})",
            LOAD_SEQ.load(Relaxed)
        );
        if let Some(prev) = self.load_handle.lock().unwrap().take() {
            prev.abort();
        }
        self.cmd_tx
            .send(PlayerCommand::Stop(event_seq))
            .map_err(|_| anyhow::anyhow!("Player thread is dead"))
    }

    pub fn seek(&self, time: f64) -> anyhow::Result<()> {
        self.cmd_tx
            .send(PlayerCommand::Seek(time))
            .map_err(|_| anyhow::anyhow!("Player thread is dead"))
    }

    pub fn set_volume(&self, volume: f64) -> anyhow::Result<()> {
        self.cmd_tx
            .send(PlayerCommand::SetVolume(volume))
            .map_err(|_| anyhow::anyhow!("Player thread is dead"))
    }

    pub fn get_audio_devices(&self, req_id: Option<String>) -> anyhow::Result<()> {
        self.cmd_tx
            .send(PlayerCommand::GetAudioDevices(req_id))
            .map_err(|_| anyhow::anyhow!("Player thread is dead"))
    }

    pub fn set_audio_device(&self, device_id: String, exclusive: bool) -> anyhow::Result<()> {
        self.cmd_tx
            .send(PlayerCommand::SetAudioDevice {
                id: device_id,
                exclusive,
            })
            .map_err(|_| anyhow::anyhow!("Player thread is dead"))
    }
}
