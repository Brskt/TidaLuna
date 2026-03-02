use crate::preload;
use crate::state::{CURRENT_METADATA, CURRENT_TRACK, HTTP_CLIENT, TrackInfo};
use crate::streaming_buffer::StreamingBuffer;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Source};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(target_os = "windows")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

static LOAD_SEQ: AtomicU32 = AtomicU32::new(0);
static EVENT_SEQ: AtomicU32 = AtomicU32::new(0);
#[cfg(target_os = "windows")]
static EXCLUSIVE_STREAM_SEQ: AtomicU32 = AtomicU32::new(0);

#[cfg(target_os = "windows")]
use crate::exclusive_wasapi::{self, ExclusiveCommand, ExclusiveEvent, ExclusiveHandle};

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

enum PlayerCommand {
    LoadData(Vec<u8>, u32, u32, String),
    LoadStreaming(StreamingBuffer, u32, u32, String),
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

struct SharedCursor {
    data: Arc<Vec<u8>>,
    pos: u64,
}

impl SharedCursor {
    fn new(data: Arc<Vec<u8>>) -> Self {
        Self { data, pos: 0 }
    }
}

impl Read for SharedCursor {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = &self.data[self.pos as usize..];
        let n = buf.len().min(available.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SharedCursor {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let len = self.data.len() as i64;
        let new_pos = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(p) => len + p,
            SeekFrom::Current(p) => self.pos as i64 + p,
        };
        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

fn format_sample_rate(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        format!("{} kHz", rate / 1000)
    } else {
        format!("{:.1} kHz", rate as f64 / 1000.0)
    }
}

fn enumerate_audio_devices() -> Vec<AudioDevice> {
    use rodio::DeviceTrait;

    let host = rodio::cpal::default_host();
    use rodio::cpal::traits::HostTrait;

    let mut devices = vec![AudioDevice {
        controllable_volume: true,
        id: "default".to_string(),
        name: "System Default".to_string(),
        r#type: Some("systemDefault".to_string()),
    }];

    if let Ok(output_devices) = host.output_devices() {
        for device in output_devices {
            if let Ok(desc) = device.description() {
                devices.push(AudioDevice {
                    controllable_volume: true,
                    id: desc.name().to_string(),
                    name: desc.name().to_string(),
                    r#type: None,
                });
            }
        }
    }

    devices
}

fn find_output_device(device_id: &str) -> Option<rodio::Device> {
    use rodio::DeviceTrait;
    use rodio::cpal::traits::HostTrait;

    let host = rodio::cpal::default_host();
    if device_id == "default" {
        return host.default_output_device();
    }

    host.output_devices().ok()?.find(|d| {
        d.description()
            .ok()
            .map(|desc| desc.name() == device_id)
            .unwrap_or(false)
    })
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

fn format_duration_mmss(secs: f64) -> String {
    let total = secs as u32;
    format!("{}:{:02}", total / 60, total % 60)
}

const RESUME_FLUSH_INTERVAL: Duration = Duration::from_millis(1200);
const RESUME_MIN_SECONDS: f64 = 1.0;

fn canonical_track_id(url: &str) -> String {
    url.split('?').next().unwrap_or(url).to_string()
}

fn resume_store_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base)
            .join("tidal-rs")
            .join("resume_positions.json")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let base = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base)
            .join(".local")
            .join("share")
            .join("tidal-rs")
            .join("resume_positions.json")
    }
}

struct ResumeStore {
    path: PathBuf,
    positions: HashMap<String, f64>,
    dirty: bool,
    last_flush: Instant,
}

impl ResumeStore {
    fn load() -> Self {
        let path = resume_store_path();
        let positions = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<HashMap<String, f64>>(&bytes).ok())
            .map(|map| {
                map.into_iter()
                    .filter(|(_, v)| v.is_finite() && *v > RESUME_MIN_SECONDS)
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();

        Self {
            path,
            positions,
            dirty: false,
            last_flush: Instant::now(),
        }
    }

    fn get(&self, track_id: &str) -> Option<f64> {
        self.positions
            .get(track_id)
            .copied()
            .filter(|v| v.is_finite() && *v > RESUME_MIN_SECONDS)
    }

    fn set(&mut self, track_id: &str, seconds: f64) {
        if !seconds.is_finite() || seconds <= RESUME_MIN_SECONDS {
            return;
        }
        let old = self.positions.get(track_id).copied().unwrap_or(0.0);
        if (old - seconds).abs() >= 0.25 {
            self.positions.insert(track_id.to_string(), seconds);
            self.dirty = true;
        }
    }

    fn clear(&mut self, track_id: &str) {
        if self.positions.remove(track_id).is_some() {
            self.dirty = true;
        }
    }

    fn flush_if_due(&mut self, force: bool) {
        if !self.dirty {
            return;
        }
        if !force && self.last_flush.elapsed() < RESUME_FLUSH_INTERVAL {
            return;
        }

        if let Some(parent) = self.path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            eprintln!("[RESUME] Failed to create state directory: {e}");
            return;
        }

        match serde_json::to_vec(&self.positions) {
            Ok(buf) => {
                if let Err(e) = fs::write(&self.path, buf) {
                    eprintln!("[RESUME] Failed to persist state: {e}");
                } else {
                    self.dirty = false;
                    self.last_flush = Instant::now();
                }
            }
            Err(e) => eprintln!("[RESUME] Failed to serialize state: {e}"),
        }
    }
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

fn open_device_sink(device_id: Option<&str>) -> anyhow::Result<MixerDeviceSink> {
    if let Some(id) = device_id
        && id != "default"
    {
        if let Some(device) = find_output_device(id) {
            return DeviceSinkBuilder::from_device(device)
                .map_err(|e| anyhow::anyhow!("Failed to configure device '{}': {}", id, e))?
                .open_stream()
                .map_err(|e| anyhow::anyhow!("Failed to open device '{}': {}", id, e));
        }
        eprintln!("[WARN] Device '{}' not found, falling back to default", id);
    }
    DeviceSinkBuilder::open_default_sink()
        .map_err(|e| anyhow::anyhow!("Failed to open default audio output: {}", e))
}

impl Player {
    pub fn new<F>(callback: F, rt_handle: tokio::runtime::Handle) -> anyhow::Result<Self>
    where
        F: Fn(PlayerEvent) + Send + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel::<PlayerCommand>();

        thread::spawn(move || {
            let mut device_sink = match open_device_sink(None) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Failed to initialize audio output: {}", e);
                    return;
                }
            };
            device_sink.log_on_drop(false);

            let mut rodio_player: Option<rodio::Player> = None;
            let mut current_duration = 0.0f64;
            let mut is_playing = false;
            let mut has_track = false;
            let mut last_empty = true;
            let mut current_data: Option<Arc<Vec<u8>>> = None;
            let mut current_streaming_buffer: Option<StreamingBuffer> = None;
            let mut current_volume: f32 = 1.0;
            let mut current_seq: u32 = 0;
            let mut current_track_id: Option<String> = None;
            // Seek requested before player/decoder is ready.
            let mut deferred_seek: Option<f64> = None;
            let mut resume_store = ResumeStore::load();

            #[cfg(target_os = "windows")]
            let mut exclusive_handle: Option<ExclusiveHandle> = None;
            #[cfg(target_os = "windows")]
            let mut is_exclusive_mode = false;
            #[cfg(target_os = "windows")]
            let mut exclusive_stream_cancel: Option<Arc<AtomicBool>> = None;

            let mut pending_cmds: Vec<PlayerCommand> = Vec::new();
            let mut coalesced_cmds: Vec<PlayerCommand> = Vec::new();
            loop {
                // Drain any commands that arrived during processing
                while let Ok(cmd) = cmd_rx.try_recv() {
                    pending_cmds.push(cmd);
                }

                // Coalesce seek bursts: keep only the newest seek until a non-seek command.
                coalesced_cmds.clear();
                let mut pending_seek: Option<f64> = None;
                for cmd in pending_cmds.drain(..) {
                    match cmd {
                        PlayerCommand::Seek(time) => {
                            pending_seek = Some(time);
                        }
                        other => {
                            if let Some(time) = pending_seek.take() {
                                coalesced_cmds.push(PlayerCommand::Seek(time));
                            }
                            coalesced_cmds.push(other);
                        }
                    }
                }
                if let Some(time) = pending_seek.take() {
                    coalesced_cmds.push(PlayerCommand::Seek(time));
                }

                for cmd in coalesced_cmds.drain(..) {
                    match cmd {
                        PlayerCommand::LoadData(data, load_gen, event_seq, track_id) => {
                            if load_gen != LOAD_SEQ.load(Relaxed) {
                                crate::vprintln!("[LOAD #{load_gen}] stale LoadData, ignoring");
                                continue;
                            }
                            current_track_id = Some(track_id.clone());
                            if deferred_seek.is_none() {
                                deferred_seek = resume_store.get(&track_id);
                            }
                            current_seq = event_seq;
                            crate::state::GOVERNOR.reset_buffer_progress();

                            #[cfg(target_os = "windows")]
                            if let Some(cancel) = exclusive_stream_cancel.take() {
                                cancel.store(true, Relaxed);
                            }

                            if let Some(ref old_buf) = current_streaming_buffer {
                                old_buf.cancel();
                            }
                            current_streaming_buffer = None;

                            let data = Arc::new(data);
                            let decode_start = std::time::Instant::now();

                            #[cfg(target_os = "windows")]
                            {
                                if is_exclusive_mode {
                                    if let Some(ref handle) = exclusive_handle {
                                        handle.send(ExclusiveCommand::Stop);
                                        let cancel = Arc::new(AtomicBool::new(false));
                                        exclusive_stream_cancel = Some(cancel.clone());

                                        let cmd_tx = handle.command_sender();
                                        let reader = SharedCursor::new(data.clone());
                                        let total_len = data.len() as u64;
                                        let stream_id =
                                            EXCLUSIVE_STREAM_SEQ.fetch_add(1, Relaxed) + 1;
                                        thread::spawn(move || {
                                            if let Err(e) =
                                                exclusive_wasapi::stream_flac_reader_to_wasapi(
                                                    reader,
                                                    total_len,
                                                    stream_id,
                                                    cmd_tx,
                                                    cancel.clone(),
                                                )
                                                && !cancel.load(Relaxed)
                                            {
                                                eprintln!(
                                                    "[WASAPI] Progressive decode failed: {e}"
                                                );
                                            }
                                        });

                                        crate::vprintln!(
                                            "[WASAPI] Progressive decode started ({:.0}ms setup)",
                                            decode_start.elapsed().as_secs_f64() * 1000.0
                                        );
                                        current_data = Some(data);
                                        current_streaming_buffer = None;
                                        has_track = true;
                                        is_playing = true;
                                    }
                                    continue;
                                }
                            }

                            let byte_len = data.len() as u64;

                            let decoder_start = std::time::Instant::now();
                            let cursor = SharedCursor::new(data.clone());
                            match Decoder::builder()
                                .with_data(cursor)
                                .with_byte_len(byte_len)
                                .with_seekable(true)
                                .with_hint("flac")
                                .build()
                            {
                                Ok(source) => {
                                    let decoder_ms = decoder_start.elapsed().as_secs_f64() * 1000.0;
                                    let source_sample_rate = source.sample_rate().get();
                                    let source_channels = source.channels().get();
                                    let source_duration = source
                                        .total_duration()
                                        .map(|d| d.as_secs_f64())
                                        .unwrap_or(0.0);

                                    // Drop previous player to disconnect from mixer
                                    if let Some(old) = rodio_player.take() {
                                        old.stop();
                                        drop(old);
                                    }

                                    let player = rodio::Player::connect_new(device_sink.mixer());
                                    player.set_volume(current_volume);
                                    player.append(source);
                                    player.pause();

                                    current_duration = source_duration;
                                    is_playing = false;
                                    has_track = true;
                                    last_empty = false;
                                    current_data = Some(data);
                                    rodio_player = Some(player);

                                    let mut initial_time = 0.0f64;
                                    if let Some(seek_time) = deferred_seek.take()
                                        && let Some(ref p) = rodio_player
                                    {
                                        if p.try_seek(Duration::from_secs_f64(seek_time)).is_ok() {
                                            initial_time = seek_time.max(0.0);
                                        } else {
                                            deferred_seek = Some(seek_time);
                                        }
                                    }

                                    if current_duration > 0.0 {
                                        callback(PlayerEvent::Duration(
                                            current_duration,
                                            current_seq,
                                        ));
                                    }
                                    callback(PlayerEvent::TimeUpdate(initial_time, current_seq));

                                    let bitrate = if current_duration > 0.0 {
                                        (byte_len as f64 * 8.0 / current_duration / 1000.0) as u32
                                    } else {
                                        0
                                    };
                                    crate::vprintln!(
                                        "[CODEC]  {} / {}ch | {} kbps",
                                        format_sample_rate(source_sample_rate),
                                        source_channels,
                                        bitrate
                                    );
                                    crate::vprintln!(
                                        "[AUDIO]  Duration: {} | Probe: n/a | Decoder: {} | Total: {}",
                                        format_duration_mmss(current_duration),
                                        format_ms(decoder_ms),
                                        format_ms(decode_start.elapsed().as_secs_f64() * 1000.0)
                                    );
                                }
                                Err(e) => {
                                    eprintln!("[ERROR]  Decode failed: {}", e);
                                }
                            }
                        }
                        PlayerCommand::LoadStreaming(buffer, load_gen, event_seq, track_id) => {
                            if load_gen != LOAD_SEQ.load(Relaxed) {
                                crate::vprintln!(
                                    "[LOAD #{load_gen}] stale LoadStreaming, cancelling"
                                );
                                buffer.cancel();
                                continue;
                            }
                            current_track_id = Some(track_id.clone());
                            if deferred_seek.is_none() {
                                deferred_seek = resume_store.get(&track_id);
                            }
                            current_seq = event_seq;
                            #[cfg(target_os = "windows")]
                            if let Some(cancel) = exclusive_stream_cancel.take() {
                                cancel.store(true, Relaxed);
                            }

                            if let Some(ref old_buf) = current_streaming_buffer {
                                old_buf.cancel();
                            }

                            let decode_start = std::time::Instant::now();

                            #[cfg(target_os = "windows")]
                            {
                                if is_exclusive_mode {
                                    if let Some(ref handle) = exclusive_handle {
                                        handle.send(ExclusiveCommand::Stop);

                                        let cancel = Arc::new(AtomicBool::new(false));
                                        exclusive_stream_cancel = Some(cancel.clone());

                                        let cmd_tx = handle.command_sender();
                                        let stream_reader = buffer.new_reader();
                                        let total_len = buffer.total_len();
                                        let stream_id =
                                            EXCLUSIVE_STREAM_SEQ.fetch_add(1, Relaxed) + 1;
                                        thread::spawn(move || {
                                            if let Err(e) =
                                                exclusive_wasapi::stream_flac_reader_to_wasapi(
                                                    stream_reader,
                                                    total_len,
                                                    stream_id,
                                                    cmd_tx,
                                                    cancel.clone(),
                                                )
                                                && !cancel.load(Relaxed)
                                            {
                                                eprintln!("[WASAPI] Stream decode failed: {e}");
                                            }
                                        });

                                        crate::vprintln!(
                                            "[WASAPI] Progressive streaming decode started ({:.0}ms setup)",
                                            decode_start.elapsed().as_secs_f64() * 1000.0
                                        );
                                        current_data = None;
                                        current_streaming_buffer = Some(buffer);
                                        has_track = true;
                                        is_playing = true;
                                    } else {
                                        buffer.cancel();
                                    }
                                    continue;
                                }
                            }

                            let total_len = buffer.total_len();

                            let reader = buffer.new_reader();
                            let decoder_start = std::time::Instant::now();
                            match Decoder::builder()
                                .with_data(reader)
                                .with_byte_len(total_len)
                                .with_seekable(true)
                                .with_hint("flac")
                                .build()
                            {
                                Ok(source) => {
                                    let decoder_ms = decoder_start.elapsed().as_secs_f64() * 1000.0;

                                    // Capture Source trait values before append consumes source
                                    let source_sample_rate = source.sample_rate().get();
                                    let source_channels = source.channels().get();
                                    let source_duration = source
                                        .total_duration()
                                        .map(|d| d.as_secs_f64())
                                        .unwrap_or(0.0);

                                    // Drop previous player to disconnect from mixer
                                    if let Some(old) = rodio_player.take() {
                                        old.stop();
                                        drop(old);
                                    }

                                    let player = rodio::Player::connect_new(device_sink.mixer());
                                    player.set_volume(current_volume);
                                    player.append(source);
                                    player.pause();

                                    current_duration = source_duration;
                                    is_playing = false;
                                    has_track = true;
                                    last_empty = false;
                                    current_data = None;
                                    current_streaming_buffer = Some(buffer);
                                    rodio_player = Some(player);

                                    let mut initial_time = 0.0f64;
                                    if let Some(seek_time) = deferred_seek.take()
                                        && let Some(ref p) = rodio_player
                                    {
                                        if p.try_seek(Duration::from_secs_f64(seek_time)).is_ok() {
                                            initial_time = seek_time.max(0.0);
                                        } else {
                                            deferred_seek = Some(seek_time);
                                        }
                                    }

                                    if current_duration > 0.0 {
                                        callback(PlayerEvent::Duration(
                                            current_duration,
                                            current_seq,
                                        ));
                                    }
                                    callback(PlayerEvent::TimeUpdate(initial_time, current_seq));

                                    let bitrate = if current_duration > 0.0 {
                                        (total_len as f64 * 8.0 / current_duration / 1000.0) as u32
                                    } else {
                                        0
                                    };
                                    // Publish bitrate (bytes/sec) for governor hysteresis.
                                    let bitrate_bps = if current_duration > 0.0 {
                                        (total_len as f64 / current_duration) as u64
                                    } else {
                                        0
                                    };
                                    crate::state::GOVERNOR
                                        .buffer_progress()
                                        .bitrate_bps
                                        .store(bitrate_bps, std::sync::atomic::Ordering::Relaxed);
                                    crate::vprintln!(
                                        "[CODEC]  {} / {}ch | {} kbps",
                                        format_sample_rate(source_sample_rate),
                                        source_channels,
                                        bitrate
                                    );
                                    crate::vprintln!(
                                        "[AUDIO]  Duration: {} | Probe: n/a | Decoder: {} | Total: {} (streaming)",
                                        format_duration_mmss(current_duration),
                                        format_ms(decoder_ms),
                                        format_ms(decode_start.elapsed().as_secs_f64() * 1000.0)
                                    );
                                }
                                Err(e) => {
                                    eprintln!("[ERROR]  Streaming decode failed: {}", e);
                                    buffer.cancel();
                                    current_streaming_buffer = None;
                                }
                            }
                        }
                        PlayerCommand::Play => {
                            #[cfg(target_os = "windows")]
                            {
                                if is_exclusive_mode {
                                    if let Some(ref handle) = exclusive_handle {
                                        handle.send(ExclusiveCommand::Play);
                                    }
                                    is_playing = true;
                                    crate::state::GOVERNOR
                                        .buffer_progress()
                                        .set_playback_active(true);
                                    continue;
                                }
                            }
                            if let Some(ref p) = rodio_player {
                                p.play();
                            }
                            is_playing = true;
                            crate::state::GOVERNOR
                                .buffer_progress()
                                .set_playback_active(true);
                            callback(PlayerEvent::StateChange("active", current_seq));
                        }
                        PlayerCommand::Pause => {
                            #[cfg(target_os = "windows")]
                            {
                                if is_exclusive_mode {
                                    if let Some(ref handle) = exclusive_handle {
                                        handle.send(ExclusiveCommand::Pause);
                                    }
                                    is_playing = false;
                                    crate::state::GOVERNOR
                                        .buffer_progress()
                                        .set_playback_active(false);
                                    resume_store.flush_if_due(true);
                                    continue;
                                }
                            }
                            if let Some(ref p) = rodio_player {
                                p.pause();
                            }
                            is_playing = false;
                            crate::state::GOVERNOR
                                .buffer_progress()
                                .set_playback_active(false);
                            callback(PlayerEvent::StateChange("paused", current_seq));
                            resume_store.flush_if_due(true);
                        }
                        PlayerCommand::Stop(event_seq) => {
                            current_seq = event_seq;
                            crate::state::GOVERNOR.reset_buffer_progress();

                            #[cfg(target_os = "windows")]
                            if let Some(cancel) = exclusive_stream_cancel.take() {
                                cancel.store(true, Relaxed);
                            }

                            if let Some(ref old_buf) = current_streaming_buffer {
                                old_buf.cancel();
                            }
                            current_streaming_buffer = None;

                            #[cfg(target_os = "windows")]
                            {
                                if is_exclusive_mode {
                                    if let Some(ref handle) = exclusive_handle {
                                        handle.send(ExclusiveCommand::Stop);
                                    }
                                    callback(PlayerEvent::TimeUpdate(0.0, current_seq));
                                    callback(PlayerEvent::StateChange("paused", current_seq));
                                    is_playing = false;
                                    has_track = false;
                                    current_duration = 0.0;
                                    current_data = None;
                                    current_track_id = None;
                                    deferred_seek = None;
                                    resume_store.flush_if_due(true);

                                    continue;
                                }
                            }
                            if let Some(old) = rodio_player.take() {
                                old.stop();
                                drop(old);
                            }
                            callback(PlayerEvent::TimeUpdate(0.0, current_seq));
                            callback(PlayerEvent::StateChange("paused", current_seq));
                            is_playing = false;
                            has_track = false;
                            current_duration = 0.0;
                            current_data = None;
                            current_track_id = None;
                            deferred_seek = None;
                            resume_store.flush_if_due(true);
                        }
                        PlayerCommand::Seek(time) => {
                            // Latest-seek-wins: drop stale seeks queued while we were busy.
                            let mut latest_time = time;
                            while let Ok(next_cmd) = cmd_rx.try_recv() {
                                match next_cmd {
                                    PlayerCommand::Seek(t) => {
                                        latest_time = t;
                                    }
                                    other => pending_cmds.push(other),
                                }
                            }
                            if let Some(track_id) = current_track_id.as_ref() {
                                resume_store.set(track_id, latest_time);
                                resume_store.flush_if_due(false);
                            }

                            #[cfg(target_os = "windows")]
                            {
                                if is_exclusive_mode {
                                    if has_track {
                                        if let Some(ref handle) = exclusive_handle {
                                            handle.send(ExclusiveCommand::Seek(latest_time));
                                        }
                                    } else {
                                        deferred_seek = Some(latest_time);
                                    }
                                    continue;
                                }
                            }
                            if let Some(ref p) = rodio_player {
                                let seek_t0 = std::time::Instant::now();
                                let result = p.try_seek(Duration::from_secs_f64(latest_time));
                                let elapsed = seek_t0.elapsed();
                                let timing = if elapsed.as_millis() > 0 {
                                    format!("{:.0}ms", elapsed.as_secs_f64() * 1000.0)
                                } else {
                                    format!("{}µs", elapsed.as_micros())
                                };
                                match result {
                                    Ok(()) => {
                                        deferred_seek = None;
                                        crate::vprintln!("[SEEK]   try_seek OK: {timing}");
                                    }
                                    Err(e) => {
                                        eprintln!("[SEEK]   try_seek FAILED ({timing}): {e}");
                                        deferred_seek = Some(latest_time);
                                    }
                                }
                            } else {
                                deferred_seek = Some(latest_time);
                                crate::vprintln!("[SEEK]   queued until player ready");
                            }
                        }
                        PlayerCommand::SetVolume(vol) => {
                            current_volume = (vol / 100.0) as f32;
                            #[cfg(target_os = "windows")]
                            {
                                if is_exclusive_mode {
                                    // Volume ignored in exclusive mode (100% forced)
                                    continue;
                                }
                            }
                            if let Some(ref p) = rodio_player {
                                p.set_volume(current_volume);
                            }
                        }
                        PlayerCommand::GetAudioDevices(req_id) => {
                            let devices = enumerate_audio_devices();
                            callback(PlayerEvent::AudioDevices(devices, req_id));
                        }
                        PlayerCommand::SetAudioDevice { id, exclusive } => {
                            let _ = &exclusive; // used on Windows only
                            #[cfg(target_os = "windows")]
                            {
                                if let Some(cancel) = exclusive_stream_cancel.take() {
                                    cancel.store(true, Relaxed);
                                }
                                if exclusive {
                                    // Switch to exclusive WASAPI mode
                                    // Stop rodio playback
                                    if let Some(ref p) = rodio_player {
                                        p.stop();
                                    }
                                    rodio_player = None;

                                    // Shutdown previous exclusive handle if any
                                    if let Some(old) = exclusive_handle.take() {
                                        old.shutdown();
                                    }

                                    let handle = ExclusiveHandle::spawn(id.clone());
                                    is_exclusive_mode = true;
                                    exclusive_handle = Some(handle);

                                    // Reload current track in exclusive mode.
                                    if let Some(ref handle) = exclusive_handle {
                                        if let Some(ref streaming_buf) = current_streaming_buffer {
                                            handle.send(ExclusiveCommand::Stop);
                                            let cancel = Arc::new(AtomicBool::new(false));
                                            exclusive_stream_cancel = Some(cancel.clone());

                                            let cmd_tx = handle.command_sender();
                                            let reader = streaming_buf.new_reader();
                                            let total_len = streaming_buf.total_len();
                                            let stream_id =
                                                EXCLUSIVE_STREAM_SEQ.fetch_add(1, Relaxed) + 1;
                                            thread::spawn(move || {
                                                if let Err(e) =
                                                    exclusive_wasapi::stream_flac_reader_to_wasapi(
                                                        reader,
                                                        total_len,
                                                        stream_id,
                                                        cmd_tx,
                                                        cancel.clone(),
                                                    )
                                                    && !cancel.load(Relaxed)
                                                {
                                                    eprintln!(
                                                        "[WASAPI] Device switch stream decode failed: {e}"
                                                    );
                                                }
                                            });
                                            has_track = true;
                                            is_playing = true;
                                        } else if let Some(ref data) = current_data {
                                            handle.send(ExclusiveCommand::Stop);
                                            let cancel = Arc::new(AtomicBool::new(false));
                                            exclusive_stream_cancel = Some(cancel.clone());

                                            let cmd_tx = handle.command_sender();
                                            let reader = SharedCursor::new(data.clone());
                                            let total_len = data.len() as u64;
                                            let stream_id =
                                                EXCLUSIVE_STREAM_SEQ.fetch_add(1, Relaxed) + 1;
                                            thread::spawn(move || {
                                                if let Err(e) =
                                                    exclusive_wasapi::stream_flac_reader_to_wasapi(
                                                        reader,
                                                        total_len,
                                                        stream_id,
                                                        cmd_tx,
                                                        cancel.clone(),
                                                    )
                                                    && !cancel.load(Relaxed)
                                                {
                                                    eprintln!(
                                                        "[WASAPI] Device switch stream decode failed: {e}"
                                                    );
                                                }
                                            });
                                            has_track = true;
                                            is_playing = true;
                                        }
                                    }

                                    crate::vprintln!(
                                        "[AUDIO] Switched to exclusive WASAPI: {}",
                                        id
                                    );
                                    continue;
                                } else if is_exclusive_mode {
                                    // Switch back from exclusive to shared mode
                                    if let Some(old) = exclusive_handle.take() {
                                        old.shutdown();
                                    }
                                    is_exclusive_mode = false;
                                    crate::vprintln!("[AUDIO] Switched back to shared mode");
                                    // Fall through to shared device setup below
                                }
                            }

                            let position = rodio_player.as_ref().map(|p| p.get_pos());
                            let was_playing = is_playing;

                            match open_device_sink(Some(&id)) {
                                Ok(mut new_sink) => {
                                    new_sink.log_on_drop(false);

                                    // Stop old player
                                    if let Some(ref p) = rodio_player {
                                        p.stop();
                                    }

                                    device_sink = new_sink;

                                    // Reload current track on new device
                                    if let Some(ref streaming_buf) = current_streaming_buffer {
                                        if streaming_buf.is_complete() {
                                            if let Some(data) = streaming_buf.take_data() {
                                                let data = Arc::new(data);
                                                let byte_len = data.len() as u64;
                                                let cursor = SharedCursor::new(data.clone());
                                                if let Ok(source) = Decoder::builder()
                                                    .with_data(cursor)
                                                    .with_byte_len(byte_len)
                                                    .with_seekable(true)
                                                    .with_hint("flac")
                                                    .build()
                                                {
                                                    let player = rodio::Player::connect_new(
                                                        device_sink.mixer(),
                                                    );
                                                    player.set_volume(current_volume);
                                                    player.append(source);

                                                    if let Some(pos) = position {
                                                        let _ = player.try_seek(pos);
                                                    }
                                                    if !was_playing {
                                                        player.pause();
                                                    }

                                                    current_data = Some(data);
                                                    current_streaming_buffer = None;
                                                    rodio_player = Some(player);
                                                }
                                            }
                                        } else {
                                            let total_len = streaming_buf.total_len();
                                            let reader = streaming_buf.new_reader();
                                            if let Ok(source) = Decoder::builder()
                                                .with_data(reader)
                                                .with_byte_len(total_len)
                                                .with_seekable(true)
                                                .with_hint("flac")
                                                .build()
                                            {
                                                let player =
                                                    rodio::Player::connect_new(device_sink.mixer());
                                                player.set_volume(current_volume);
                                                player.append(source);

                                                if let Some(pos) = position {
                                                    let _ = player.try_seek(pos);
                                                }
                                                if !was_playing {
                                                    player.pause();
                                                }

                                                rodio_player = Some(player);
                                            }
                                        }
                                    } else if let Some(ref data) = current_data {
                                        let byte_len = data.len() as u64;
                                        let cursor = SharedCursor::new(data.clone());
                                        if let Ok(source) = Decoder::builder()
                                            .with_data(cursor)
                                            .with_byte_len(byte_len)
                                            .with_seekable(true)
                                            .with_hint("flac")
                                            .build()
                                        {
                                            let player =
                                                rodio::Player::connect_new(device_sink.mixer());
                                            player.set_volume(current_volume);
                                            player.append(source);

                                            if let Some(pos) = position {
                                                let _ = player.try_seek(pos);
                                            }

                                            if !was_playing {
                                                player.pause();
                                            }

                                            rodio_player = Some(player);
                                        }
                                    }

                                    crate::vprintln!("[AUDIO] Switched to device: {}", id);
                                }
                                Err(e) => {
                                    eprintln!("[ERROR] Failed to switch to device '{}': {}", id, e);
                                }
                            }
                        }
                    }
                }

                // Poll exclusive WASAPI events
                #[cfg(target_os = "windows")]
                {
                    if is_exclusive_mode {
                        if let Some(ref handle) = exclusive_handle {
                            for ev in handle.poll_events() {
                                match ev {
                                    ExclusiveEvent::TimeUpdate(t) => {
                                        if let Some(track_id) = current_track_id.as_ref() {
                                            resume_store.set(track_id, t);
                                            resume_store.flush_if_due(false);
                                        }
                                        callback(PlayerEvent::TimeUpdate(t, current_seq));
                                    }
                                    ExclusiveEvent::StateChange(s) => {
                                        if s == "completed" {
                                            if let Some(track_id) = current_track_id.as_ref() {
                                                resume_store.clear(track_id);
                                                resume_store.flush_if_due(true);
                                            }
                                            has_track = false;
                                            is_playing = false;
                                            current_track_id = None;
                                            crate::state::GOVERNOR
                                                .buffer_progress()
                                                .set_playback_active(false);
                                        }
                                        callback(PlayerEvent::StateChange(s, current_seq));
                                    }
                                    ExclusiveEvent::Duration(d) => {
                                        current_duration = d;
                                        callback(PlayerEvent::Duration(d, current_seq));
                                        if let Some(seek_time) = deferred_seek.take() {
                                            handle.send(ExclusiveCommand::Seek(seek_time));
                                            callback(PlayerEvent::TimeUpdate(
                                                seek_time,
                                                current_seq,
                                            ));
                                        }
                                    }
                                    ExclusiveEvent::InitFailed(e) => {
                                        eprintln!(
                                            "[WASAPI] Init failed, falling back to shared mode: {e}"
                                        );
                                        if let Some(cancel) = exclusive_stream_cancel.take() {
                                            cancel.store(true, Relaxed);
                                        }
                                        is_exclusive_mode = false;
                                        // Handle will be dropped
                                    }
                                    ExclusiveEvent::Stopped => {
                                        has_track = false;
                                        is_playing = false;
                                        current_track_id = None;
                                        crate::state::GOVERNOR
                                            .buffer_progress()
                                            .set_playback_active(false);
                                        resume_store.flush_if_due(true);
                                    }
                                }
                            }
                        }

                        // If we got InitFailed, drop the handle
                        if !is_exclusive_mode {
                            exclusive_handle = None;
                        }
                    }
                }

                // Poll playback state (shared/rodio mode)
                #[cfg(target_os = "windows")]
                let should_poll_rodio = !is_exclusive_mode;
                #[cfg(not(target_os = "windows"))]
                let should_poll_rodio = true;

                if should_poll_rodio
                    && has_track
                    && is_playing
                    && let Some(ref p) = rodio_player
                {
                    let pos = p.get_pos();
                    let pos_secs = pos.as_secs_f64();

                    if pos_secs > 0.0 {
                        if let Some(track_id) = current_track_id.as_ref() {
                            resume_store.set(track_id, pos_secs);
                            resume_store.flush_if_due(false);
                        }
                        callback(PlayerEvent::TimeUpdate(pos_secs, current_seq));
                    }

                    if p.empty() && !last_empty {
                        if let Some(track_id) = current_track_id.as_ref() {
                            resume_store.clear(track_id);
                            resume_store.flush_if_due(true);
                        }
                        callback(PlayerEvent::TimeUpdate(current_duration, current_seq));
                        callback(PlayerEvent::StateChange("completed", current_seq));
                        has_track = false;
                        is_playing = false;
                        current_track_id = None;
                        crate::state::GOVERNOR
                            .buffer_progress()
                            .set_playback_active(false);
                        current_duration = 0.0;
                        last_empty = true;
                    }
                }

                // Wait for next command or poll timeout (replaces sleep for lower seek latency)
                if let Ok(cmd) = cmd_rx.recv_timeout(Duration::from_millis(250)) {
                    pending_cmds.push(cmd);
                    // Drain any additional queued commands
                    while let Ok(cmd) = cmd_rx.try_recv() {
                        pending_cmds.push(cmd);
                    }
                }
            }
        });

        Ok(Self {
            cmd_tx,
            rt_handle,
            load_handle: std::sync::Mutex::new(None),
        })
    }

    pub fn load(&self, url: String, format: String, key: String) -> anyhow::Result<()> {
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
            ));

            // Download continues in background, writer feeds the buffer
            preload::start_streaming_download(resp, url, key, writer);
        });

        *self.load_handle.lock().unwrap() = Some(handle);

        Ok(())
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
