use crate::preload;
use crate::state::{CURRENT_METADATA, CURRENT_TRACK, HTTP_CLIENT, TrackInfo};
use crate::streaming_buffer::StreamingBuffer;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Source};
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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
    TimeUpdate(f64),
    Duration(f64),
    StateChange(String),
    AudioDevices(Vec<AudioDevice>, Option<String>),
}

enum PlayerCommand {
    LoadData(Vec<u8>),
    LoadStreaming(StreamingBuffer),
    Play,
    Pause,
    Stop,
    Seek(f64),
    SetVolume(f64),
    GetAudioDevices(Option<String>),
    SetAudioDevice { id: String, exclusive: bool },
}

pub struct Player {
    cmd_tx: mpsc::Sender<PlayerCommand>,
    rt_handle: tokio::runtime::Handle,
}

struct AudioInfo {
    duration: f64,
    sample_rate: u32,
    bits_per_sample: u32,
    channels: u16,
}

struct MediaSourceAdapter<R> {
    inner: R,
    byte_len: u64,
}

impl<R: Read + Seek + Send + Sync> symphonia::core::io::MediaSource for MediaSourceAdapter<R> {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.byte_len)
    }
}

impl<R: Read> Read for MediaSourceAdapter<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R: Seek> Seek for MediaSourceAdapter<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

fn get_audio_info<R: Read + Seek + Send + Sync + 'static>(
    reader: R,
    byte_len: u64,
) -> Option<AudioInfo> {
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::probe::Hint;

    let adapter = MediaSourceAdapter {
        inner: reader,
        byte_len,
    };
    let mss = MediaSourceStream::new(Box::new(adapter), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("flac");

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &Default::default(), &Default::default())
        .ok()?;
    let track = probed.format.default_track()?;
    let params = &track.codec_params;

    let n_frames = params.n_frames?;
    let sample_rate = params.sample_rate?;
    let duration = n_frames as f64 / sample_rate as f64;
    let bits_per_sample = params.bits_per_sample.unwrap_or(0);
    let channels = params.channels.map(|c| c.count() as u16).unwrap_or(0);

    Some(AudioInfo {
        duration,
        sample_rate,
        bits_per_sample,
        channels,
    })
}

fn format_sample_rate(rate: u32) -> String {
    if rate % 1000 == 0 {
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

fn print_track_banner(format: &str) {
    let meta = CURRENT_METADATA.lock().unwrap().clone();
    let (title, artist, quality) = match meta {
        Some(m) => (
            if m.title.is_empty() {
                "Unknown".to_string()
            } else {
                m.title
            },
            if m.artist.is_empty() {
                "Unknown".to_string()
            } else {
                m.artist
            },
            if m.quality.is_empty() {
                format.to_uppercase()
            } else {
                m.quality
            },
        ),
        None => (
            "Unknown".to_string(),
            "Unknown".to_string(),
            format.to_uppercase(),
        ),
    };
    eprintln!("══════════════════════════════════════════");
    eprintln!("  {} — {}", title, artist);
    eprintln!("  Quality: {} | Format: {}", quality, format);
    eprintln!("══════════════════════════════════════════");
}

fn open_device_sink(device_id: Option<&str>) -> anyhow::Result<MixerDeviceSink> {
    if let Some(id) = device_id {
        if id != "default" {
            if let Some(device) = find_output_device(id) {
                return DeviceSinkBuilder::from_device(device)
                    .map_err(|e| anyhow::anyhow!("Failed to configure device '{}': {}", id, e))?
                    .open_stream()
                    .map_err(|e| anyhow::anyhow!("Failed to open device '{}': {}", id, e));
            }
            eprintln!("[WARN] Device '{}' not found, falling back to default", id);
        }
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
            let mut current_data: Option<Vec<u8>> = None;
            let mut current_streaming_buffer: Option<StreamingBuffer> = None;
            let mut current_volume: f32 = 1.0;

            #[cfg(target_os = "windows")]
            let mut exclusive_handle: Option<ExclusiveHandle> = None;
            #[cfg(target_os = "windows")]
            let mut is_exclusive_mode = false;

            loop {
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        PlayerCommand::LoadData(data) => {
                            if let Some(ref old_buf) = current_streaming_buffer {
                                old_buf.cancel();
                            }
                            current_streaming_buffer = None;

                            let decode_start = std::time::Instant::now();

                            #[cfg(target_os = "windows")]
                            {
                                if is_exclusive_mode {
                                    if let Some(ref handle) = exclusive_handle {
                                        match exclusive_wasapi::decode_flac_to_pcm(&data) {
                                            Ok(pcm) => {
                                                eprintln!(
                                                    "[WASAPI] PCM decode: {:.0}ms",
                                                    decode_start.elapsed().as_secs_f64() * 1000.0
                                                );
                                                handle.send(ExclusiveCommand::Load {
                                                    pcm_data: pcm.data,
                                                    sample_rate: pcm.sample_rate,
                                                    channels: pcm.channels,
                                                    bits_per_sample: pcm.bits_per_sample,
                                                    total_frames: pcm.total_frames,
                                                    duration_secs: pcm.duration_secs,
                                                });
                                                current_data = Some(data);
                                                has_track = true;
                                                is_playing = true;
                                            }
                                            Err(e) => {
                                                eprintln!("[WASAPI] PCM decode failed: {e}");
                                            }
                                        }
                                    }
                                    continue;
                                }
                            }

                            let probe_start = std::time::Instant::now();
                            let audio_info =
                                get_audio_info(Cursor::new(data.to_vec()), data.len() as u64);
                            let probe_ms = probe_start.elapsed().as_secs_f64() * 1000.0;

                            let byte_len = data.len() as u64;
                            let decoder_start = std::time::Instant::now();
                            let cursor = Cursor::new(data.clone());
                            match Decoder::builder()
                                .with_data(cursor)
                                .with_byte_len(byte_len)
                                .with_seekable(true)
                                .with_hint("flac")
                                .build()
                            {
                                Ok(source) => {
                                    let decoder_ms = decoder_start.elapsed().as_secs_f64() * 1000.0;

                                    // Drop previous player to disconnect from mixer
                                    if let Some(old) = rodio_player.take() {
                                        old.stop();
                                        drop(old);
                                    }

                                    let player = rodio::Player::connect_new(device_sink.mixer());
                                    player.set_volume(current_volume);
                                    player.append(source);
                                    player.pause();

                                    current_duration =
                                        audio_info.as_ref().map(|i| i.duration).unwrap_or(0.0);
                                    is_playing = false;
                                    has_track = true;
                                    last_empty = false;
                                    current_data = Some(data);
                                    rodio_player = Some(player);

                                    if current_duration > 0.0 {
                                        callback(PlayerEvent::Duration(current_duration));
                                    }

                                    if let Some(ref info) = audio_info {
                                        let bitrate = if info.duration > 0.0 {
                                            (byte_len as f64 * 8.0 / info.duration / 1000.0) as u32
                                        } else {
                                            0
                                        };
                                        eprintln!(
                                            "[CODEC]  {} / {}bit / {}ch | {} kbps",
                                            format_sample_rate(info.sample_rate),
                                            info.bits_per_sample,
                                            info.channels,
                                            bitrate
                                        );
                                    }
                                    eprintln!(
                                        "[AUDIO]  Duration: {} | Probe: {} | Decoder: {} | Total: {}",
                                        format_duration_mmss(current_duration),
                                        format_ms(probe_ms),
                                        format_ms(decoder_ms),
                                        format_ms(decode_start.elapsed().as_secs_f64() * 1000.0)
                                    );
                                }
                                Err(e) => {
                                    eprintln!("[ERROR]  Decode failed: {}", e);
                                }
                            }
                        }
                        PlayerCommand::LoadStreaming(buffer) => {
                            if let Some(ref old_buf) = current_streaming_buffer {
                                old_buf.cancel();
                            }

                            let decode_start = std::time::Instant::now();

                            #[cfg(target_os = "windows")]
                            {
                                if is_exclusive_mode {
                                    buffer.wait_for_complete();
                                    if let Some(data) = buffer.to_vec() {
                                        if let Some(ref handle) = exclusive_handle {
                                            match exclusive_wasapi::decode_flac_to_pcm(&data) {
                                                Ok(pcm) => {
                                                    eprintln!(
                                                        "[WASAPI] PCM decode (streamed): {:.0}ms",
                                                        decode_start.elapsed().as_secs_f64()
                                                            * 1000.0
                                                    );
                                                    handle.send(ExclusiveCommand::Load {
                                                        pcm_data: pcm.data,
                                                        sample_rate: pcm.sample_rate,
                                                        channels: pcm.channels,
                                                        bits_per_sample: pcm.bits_per_sample,
                                                        total_frames: pcm.total_frames,
                                                        duration_secs: pcm.duration_secs,
                                                    });
                                                    current_data = Some(data);
                                                    current_streaming_buffer = None;
                                                    has_track = true;
                                                    is_playing = true;
                                                }
                                                Err(e) => {
                                                    eprintln!("[WASAPI] PCM decode failed: {e}");
                                                }
                                            }
                                        }
                                    }
                                    continue;
                                }
                            }

                            let total_len = buffer.total_len();

                            let probe_start = std::time::Instant::now();
                            let audio_info = get_audio_info(buffer.new_reader(), total_len);
                            let probe_ms = probe_start.elapsed().as_secs_f64() * 1000.0;

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

                                    // Drop previous player to disconnect from mixer
                                    if let Some(old) = rodio_player.take() {
                                        old.stop();
                                        drop(old);
                                    }

                                    let duration = audio_info
                                        .as_ref()
                                        .map(|i| i.duration)
                                        .or_else(|| {
                                            source.total_duration().map(|d| d.as_secs_f64())
                                        })
                                        .unwrap_or(0.0);

                                    let player = rodio::Player::connect_new(device_sink.mixer());
                                    player.set_volume(current_volume);
                                    player.append(source);
                                    player.pause();

                                    current_duration = duration;
                                    is_playing = false;
                                    has_track = true;
                                    last_empty = false;
                                    current_data = None;
                                    current_streaming_buffer = Some(buffer);
                                    rodio_player = Some(player);

                                    if current_duration > 0.0 {
                                        callback(PlayerEvent::Duration(current_duration));
                                    }

                                    if let Some(ref info) = audio_info {
                                        let bitrate = if info.duration > 0.0 {
                                            (total_len as f64 * 8.0 / info.duration / 1000.0) as u32
                                        } else {
                                            0
                                        };
                                        eprintln!(
                                            "[CODEC]  {} / {}bit / {}ch | {} kbps",
                                            format_sample_rate(info.sample_rate),
                                            info.bits_per_sample,
                                            info.channels,
                                            bitrate
                                        );
                                    }
                                    eprintln!(
                                        "[AUDIO]  Duration: {} | Probe: {} | Decoder: {} | Total: {} (streaming)",
                                        format_duration_mmss(current_duration),
                                        format_ms(probe_ms),
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
                                    continue;
                                }
                            }
                            if let Some(ref p) = rodio_player {
                                p.play();
                            }
                            is_playing = true;
                            callback(PlayerEvent::StateChange("active".to_string()));
                        }
                        PlayerCommand::Pause => {
                            #[cfg(target_os = "windows")]
                            {
                                if is_exclusive_mode {
                                    if let Some(ref handle) = exclusive_handle {
                                        handle.send(ExclusiveCommand::Pause);
                                    }
                                    is_playing = false;
                                    continue;
                                }
                            }
                            if let Some(ref p) = rodio_player {
                                p.pause();
                            }
                            is_playing = false;
                            callback(PlayerEvent::StateChange("paused".to_string()));
                        }
                        PlayerCommand::Stop => {
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
                                    is_playing = false;
                                    has_track = false;
                                    current_data = None;
                                    continue;
                                }
                            }
                            if let Some(old) = rodio_player.take() {
                                old.stop();
                                drop(old);
                            }
                            is_playing = false;
                            has_track = false;
                            current_duration = 0.0;
                            current_data = None;
                        }
                        PlayerCommand::Seek(time) => {
                            #[cfg(target_os = "windows")]
                            {
                                if is_exclusive_mode {
                                    if let Some(ref handle) = exclusive_handle {
                                        handle.send(ExclusiveCommand::Seek(time));
                                    }
                                    continue;
                                }
                            }
                            if let Some(ref p) = rodio_player {
                                if let Err(e) = p.try_seek(Duration::from_secs_f64(time)) {
                                    eprintln!("Seek failed: {}", e);
                                }
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

                                    // Reload current track in exclusive mode
                                    if let Some(ref data) = current_data {
                                        if let Some(ref handle) = exclusive_handle {
                                            match exclusive_wasapi::decode_flac_to_pcm(data) {
                                                Ok(pcm) => {
                                                    handle.send(ExclusiveCommand::Load {
                                                        pcm_data: pcm.data,
                                                        sample_rate: pcm.sample_rate,
                                                        channels: pcm.channels,
                                                        bits_per_sample: pcm.bits_per_sample,
                                                        total_frames: pcm.total_frames,
                                                        duration_secs: pcm.duration_secs,
                                                    });
                                                    has_track = true;
                                                    is_playing = true;
                                                }
                                                Err(e) => {
                                                    eprintln!("[WASAPI] PCM decode failed: {e}");
                                                }
                                            }
                                        }
                                    }

                                    eprintln!("[AUDIO] Switched to exclusive WASAPI: {}", id);
                                    continue;
                                } else if is_exclusive_mode {
                                    // Switch back from exclusive to shared mode
                                    if let Some(old) = exclusive_handle.take() {
                                        old.shutdown();
                                    }
                                    is_exclusive_mode = false;
                                    eprintln!("[AUDIO] Switched back to shared mode");
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
                                            if let Some(data) = streaming_buf.to_vec() {
                                                let byte_len = data.len() as u64;
                                                let cursor = Cursor::new(data.clone());
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
                                        let cursor = Cursor::new(data.clone());
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

                                    eprintln!("[AUDIO] Switched to device: {}", id);
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
                                        callback(PlayerEvent::TimeUpdate(t));
                                    }
                                    ExclusiveEvent::StateChange(s) => {
                                        if s == "completed" {
                                            has_track = false;
                                            is_playing = false;
                                        }
                                        callback(PlayerEvent::StateChange(s));
                                    }
                                    ExclusiveEvent::Duration(d) => {
                                        current_duration = d;
                                        callback(PlayerEvent::Duration(d));
                                    }
                                    ExclusiveEvent::InitFailed(e) => {
                                        eprintln!(
                                            "[WASAPI] Init failed, falling back to shared mode: {e}"
                                        );
                                        is_exclusive_mode = false;
                                        // Handle will be dropped
                                    }
                                    ExclusiveEvent::Stopped => {
                                        has_track = false;
                                        is_playing = false;
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

                if should_poll_rodio && has_track && is_playing {
                    if let Some(ref p) = rodio_player {
                        let pos = p.get_pos();
                        let pos_secs = pos.as_secs_f64();

                        if pos_secs > 0.0 {
                            callback(PlayerEvent::TimeUpdate(pos_secs));
                        }

                        if p.empty() && !last_empty {
                            callback(PlayerEvent::TimeUpdate(current_duration));
                            callback(PlayerEvent::StateChange("completed".to_string()));
                            has_track = false;
                            is_playing = false;
                            current_duration = 0.0;
                            last_empty = true;
                        }
                    }
                }

                thread::sleep(Duration::from_millis(250));
            }
        });

        Ok(Self { cmd_tx, rt_handle })
    }

    pub fn load(&self, url: String, format: String, key: String) -> anyhow::Result<()> {
        print_track_banner(&format);

        {
            let mut lock = CURRENT_TRACK.lock().unwrap();
            *lock = Some(TrackInfo {
                url: url.clone(),
                key: key.clone(),
            });
        }

        let cmd_tx = self.cmd_tx.clone();
        self.rt_handle.spawn(async move {
            let load_start = std::time::Instant::now();
            let track = TrackInfo {
                url: url.clone(),
                key: key.clone(),
            };

            // Use preloaded data if available (instant)
            if let Some(preloaded) = preload::take_preloaded_if_match(&track).await {
                eprintln!(
                    "[PRELOAD] Using preloaded data ({}, {:.0}ms)",
                    format_bytes(preloaded.data.len() as u64),
                    load_start.elapsed().as_secs_f64() * 1000.0
                );
                let _ = cmd_tx.send(PlayerCommand::LoadData(preloaded.data));
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

            let total_len = resp.content_length().unwrap_or(0);
            if total_len == 0 {
                eprintln!(
                    "[FETCH]  No Content-Length, full download... (TTFB: {:.0}ms)",
                    ttfb_ms
                );
                match preload::fetch_and_decrypt(&url, &key).await {
                    Ok(data) => {
                        eprintln!(
                            "[FETCH]  Done ({} in {:.0}ms)",
                            format_bytes(data.len() as u64),
                            load_start.elapsed().as_secs_f64() * 1000.0
                        );
                        let _ = cmd_tx.send(PlayerCommand::LoadData(data));
                    }
                    Err(e) => {
                        eprintln!("[ERROR]  Fetch failed: {}", e);
                    }
                }
                return;
            }

            eprintln!(
                "[NET]    TTFB: {} | Size: {}",
                format_ms(ttfb_ms),
                format_bytes(total_len)
            );

            let (buffer, writer) = StreamingBuffer::new(total_len);
            let _ = cmd_tx.send(PlayerCommand::LoadStreaming(buffer));

            // Download continues in background, writer feeds the buffer
            preload::start_streaming_download(resp, key, writer);
        });

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
        self.cmd_tx
            .send(PlayerCommand::Stop)
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
