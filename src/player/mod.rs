pub(crate) mod ipc;
mod resume;
pub(crate) mod streaming;
mod thread;
#[cfg(target_os = "windows")]
pub(crate) mod wasapi;

use crate::preload;
use crate::state::{CURRENT_METADATA, CURRENT_TRACK, HTTP_CLIENT, TrackInfo};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::mpsc;
use streaming::StreamingBuffer;

static LOAD_SEQ: AtomicU32 = AtomicU32::new(0);
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

#[derive(Debug)]
pub enum PlayerEvent {
    TimeUpdate(f64, u32),
    Duration(f64, u32),
    StateChange(&'static str, u32),
    AudioDevices(Vec<AudioDevice>, Option<String>),
}

#[derive(Debug, Clone, Copy)]
enum ResumePolicy {
    Disabled,
    Auto,
    Explicit(f64),
}

enum PlayerCommand {
    LoadData(Vec<u8>, u32, u32, String, ResumePolicy),
    LoadStreaming(StreamingBuffer, u32, u32, String, ResumePolicy),
    Play,
    Pause,
    Stop(u32),
    Seek(f64),
    SetVolume(f64),
    GetAudioDevices(Option<String>),
    SetAudioDevice { id: String, exclusive: bool },
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
    ) -> anyhow::Result<()> {
        let load_gen = LOAD_SEQ.fetch_add(1, Relaxed) + 1;
        let event_seq = EVENT_SEQ.fetch_add(1, Relaxed) + 1;
        crate::vprintln!("[LOAD #{load_gen}] start");
        print_track_banner(&format);

        // Abort any in-flight load task
        if let Some(prev) = self.load_handle.lock().unwrap().take() {
            prev.abort();
        }

        {
            let mut lock = CURRENT_TRACK.lock().unwrap();
            *lock = Some(TrackInfo {
                url: url.clone(),
                key: key.clone(),
            });
        }

        let cmd_tx = self.cmd_tx.clone();
        let handle = self.rt_handle.spawn(async move {
            let load_start = std::time::Instant::now();
            let track = TrackInfo {
                url: url.clone(),
                key: key.clone(),
            };
            let track_id = canonical_track_id(&url);

            // Use preloaded data if available (instant)
            if let Some(preloaded) = preload::take_preloaded_if_match(&track).await {
                if LOAD_SEQ.load(Relaxed) != load_gen {
                    crate::vprintln!("[LOAD #{load_gen}] stale after preload check, dropping");
                    return;
                }
                crate::vprintln!(
                    "[PRELOAD] Using preloaded data ({}, {:.0}ms)",
                    format_bytes(preloaded.data.len() as u64),
                    load_start.elapsed().as_secs_f64() * 1000.0
                );
                let _ = cmd_tx.send(PlayerCommand::LoadData(
                    preloaded.data,
                    load_gen,
                    event_seq,
                    track_id.clone(),
                    resume_policy,
                ));
                return;
            }

            if LOAD_SEQ.load(Relaxed) != load_gen {
                crate::vprintln!("[LOAD #{load_gen}] stale before HTTP, dropping");
                return;
            }

            // Start streaming download
            let req_start = std::time::Instant::now();
            let resp = match HTTP_CLIENT.get(&url).send().await {
                Ok(r) => {
                    if !r.status().is_success() {
                        eprintln!("[ERROR]  Upstream status: {}", r.status());
                        return;
                    }
                    r
                }
                Err(e) => {
                    eprintln!("[ERROR]  Request failed: {}", e);
                    return;
                }
            };
            let ttfb_ms = req_start.elapsed().as_secs_f64() * 1000.0;

            if LOAD_SEQ.load(Relaxed) != load_gen {
                crate::vprintln!("[LOAD #{load_gen}] stale after HTTP TTFB, dropping");
                return;
            }

            let total_len = resp.content_length().unwrap_or(0);
            if total_len == 0 {
                crate::vprintln!(
                    "[FETCH]  No Content-Length, full download... (TTFB: {:.0}ms)",
                    ttfb_ms
                );
                match preload::fetch_and_decrypt(&url, &key).await {
                    Ok(data) => {
                        if LOAD_SEQ.load(Relaxed) != load_gen {
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
                        let _ = cmd_tx.send(PlayerCommand::LoadData(
                            data,
                            load_gen,
                            event_seq,
                            track_id.clone(),
                            resume_policy,
                        ));
                    }
                    Err(e) => {
                        eprintln!("[ERROR]  Fetch failed: {}", e);
                    }
                }
                return;
            }

            crate::vprintln!(
                "[NET]    TTFB: {} | Size: {} (load #{load_gen})",
                format_ms(ttfb_ms),
                format_bytes(total_len)
            );

            crate::state::GOVERNOR.reset_buffer_progress();
            let bp = crate::state::GOVERNOR.buffer_progress().clone();
            let (buffer, writer) = StreamingBuffer::new(total_len, Some(bp));
            crate::vprintln!("[LOAD #{load_gen}] sending LoadStreaming");
            let _ = cmd_tx.send(PlayerCommand::LoadStreaming(
                buffer,
                load_gen,
                event_seq,
                track_id.clone(),
                resume_policy,
            ));

            // Download continues in background, writer feeds the buffer
            preload::start_streaming_download(resp, url, key, writer);
        });

        *self.load_handle.lock().unwrap() = Some(handle);

        Ok(())
    }

    pub fn load(&self, url: String, format: String, key: String) -> anyhow::Result<()> {
        self.load_with_policy(url, format, key, ResumePolicy::Disabled)
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
        self.load_with_policy(url, format, key, policy)
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
