use crate::preload;
use crate::state::{CURRENT_TRACK, TrackInfo};
use rodio::{DeviceSinkBuilder, Decoder, MixerDeviceSink};
use std::io::Cursor;
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

fn get_audio_duration(data: &[u8]) -> Option<f64> {
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::probe::Hint;

    let cursor = Cursor::new(data.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("flac");

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &Default::default(), &Default::default())
        .ok()?;
    let track = probed.format.default_track()?;
    let n_frames = track.codec_params.n_frames?;
    let sample_rate = track.codec_params.sample_rate?;

    Some(n_frames as f64 / sample_rate as f64)
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
            let mut current_volume: f32 = 1.0;

            #[cfg(target_os = "windows")]
            let mut exclusive_handle: Option<ExclusiveHandle> = None;
            #[cfg(target_os = "windows")]
            let mut is_exclusive_mode = false;

            loop {
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        PlayerCommand::LoadData(data) => {
                            #[cfg(target_os = "windows")]
                            {
                                if is_exclusive_mode {
                                    if let Some(ref handle) = exclusive_handle {
                                        match exclusive_wasapi::decode_flac_to_pcm(&data) {
                                            Ok(pcm) => {
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

                            let duration = get_audio_duration(&data);
                            let byte_len = data.len() as u64;

                            let cursor = Cursor::new(data.clone());
                            match Decoder::builder()
                                .with_data(cursor)
                                .with_byte_len(byte_len)
                                .with_seekable(true)
                                .with_hint("flac")
                                .build()
                            {
                                Ok(source) => {
                                    // Stop previous playback
                                    if let Some(ref p) = rodio_player {
                                        p.stop();
                                    }

                                    let player = rodio::Player::connect_new(device_sink.mixer());
                                    player.set_volume(current_volume);
                                    player.append(source);

                                    current_duration = duration.unwrap_or(0.0);
                                    is_playing = true;
                                    has_track = true;
                                    last_empty = false;
                                    current_data = Some(data);
                                    rodio_player = Some(player);

                                    if current_duration > 0.0 {
                                        callback(PlayerEvent::Duration(current_duration));
                                    }
                                    callback(PlayerEvent::StateChange("active".to_string()));

                                    eprintln!(
                                        "[AUDIO] Playing: duration={:.1}s, volume={:.0}%",
                                        current_duration,
                                        current_volume * 100.0
                                    );
                                }
                                Err(e) => {
                                    eprintln!("Failed to decode audio: {}", e);
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
                            if let Some(ref p) = rodio_player {
                                p.stop();
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
                                    if let Some(ref data) = current_data {
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
                                    eprintln!(
                                        "[ERROR] Failed to switch to device '{}': {}",
                                        id, e
                                    );
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
                                        eprintln!("[WASAPI] Init failed, falling back to shared mode: {e}");
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

    pub fn load(&self, url: String, _format: String, key: String) -> anyhow::Result<()> {
        {
            let mut lock = CURRENT_TRACK.lock().unwrap();
            *lock = Some(TrackInfo {
                url: url.clone(),
                key: key.clone(),
            });
        }

        let cmd_tx = self.cmd_tx.clone();
        self.rt_handle.spawn(async move {
            let track = TrackInfo {
                url: url.clone(),
                key: key.clone(),
            };

            let data = if let Some(preloaded) = preload::take_preloaded_if_match(&track).await {
                eprintln!("[PRELOAD] Using preloaded data ({} bytes)", preloaded.data.len());
                preloaded.data
            } else {
                eprintln!("[FETCH] Downloading and decrypting track...");
                match preload::fetch_and_decrypt(&url, &key).await {
                    Ok(d) => {
                        eprintln!("[FETCH] Done ({} bytes)", d.len());
                        d
                    }
                    Err(e) => {
                        eprintln!("Failed to fetch track: {}", e);
                        return;
                    }
                }
            };

            let _ = cmd_tx.send(PlayerCommand::LoadData(data));
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
