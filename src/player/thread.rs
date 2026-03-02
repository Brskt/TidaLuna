use super::resume::{RESUME_MIN_SECONDS, ResumeStore};
use super::streaming::StreamingBuffer;
use super::{LOAD_SEQ, PlayerCommand, PlayerEvent, ResumePolicy, format_ms};
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Source};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
#[cfg(target_os = "windows")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
#[cfg(target_os = "windows")]
use std::thread;
use std::time::Duration;

#[cfg(target_os = "windows")]
use super::{EXCLUSIVE_STREAM_SEQ, wasapi};
#[cfg(target_os = "windows")]
use wasapi::{ExclusiveCommand, ExclusiveEvent, ExclusiveHandle};

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

fn format_duration_mmss(secs: f64) -> String {
    let total = secs as u32;
    format!("{}:{:02}", total / 60, total % 60)
}

fn enumerate_audio_devices() -> Vec<super::AudioDevice> {
    use rodio::DeviceTrait;

    let host = rodio::cpal::default_host();
    use rodio::cpal::traits::HostTrait;

    let mut devices = vec![super::AudioDevice {
        controllable_volume: true,
        id: "default".to_string(),
        name: "System Default".to_string(),
        r#type: Some("systemDefault".to_string()),
    }];

    if let Ok(output_devices) = host.output_devices() {
        for device in output_devices {
            if let Ok(desc) = device.description() {
                devices.push(super::AudioDevice {
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

pub(super) struct PlayerThread<F> {
    cmd_rx: mpsc::Receiver<PlayerCommand>,
    callback: F,
    device_sink: MixerDeviceSink,
    rodio_player: Option<rodio::Player>,
    current_duration: f64,
    is_playing: bool,
    has_track: bool,
    last_empty: bool,
    current_data: Option<Arc<Vec<u8>>>,
    current_streaming_buffer: Option<StreamingBuffer>,
    current_volume: f32,
    current_seq: u32,
    current_track_id: Option<String>,
    pending_resume_seek: Option<f64>,
    resume_store: ResumeStore,
    allow_startup_auto_resume: bool,
    #[cfg(target_os = "windows")]
    exclusive_handle: Option<ExclusiveHandle>,
    #[cfg(target_os = "windows")]
    is_exclusive_mode: bool,
    #[cfg(target_os = "windows")]
    exclusive_stream_cancel: Option<Arc<AtomicBool>>,
    pending_cmds: Vec<PlayerCommand>,
    coalesced_cmds: Vec<PlayerCommand>,
}

impl<F: Fn(PlayerEvent) + Send + 'static> PlayerThread<F> {
    pub fn new(cmd_rx: mpsc::Receiver<PlayerCommand>, callback: F) -> Option<Self> {
        let mut device_sink = match open_device_sink(None) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to initialize audio output: {}", e);
                return None;
            }
        };
        device_sink.log_on_drop(false);

        Some(Self {
            cmd_rx,
            callback,
            device_sink,
            rodio_player: None,
            current_duration: 0.0,
            is_playing: false,
            has_track: false,
            last_empty: true,
            current_data: None,
            current_streaming_buffer: None,
            current_volume: 1.0,
            current_seq: 0,
            current_track_id: None,
            pending_resume_seek: None,
            resume_store: ResumeStore::load(),
            allow_startup_auto_resume: true,
            #[cfg(target_os = "windows")]
            exclusive_handle: None,
            #[cfg(target_os = "windows")]
            is_exclusive_mode: false,
            #[cfg(target_os = "windows")]
            exclusive_stream_cancel: None,
            pending_cmds: Vec::new(),
            coalesced_cmds: Vec::new(),
        })
    }

    pub fn run(&mut self) {
        loop {
            // Drain any commands that arrived during processing
            while let Ok(cmd) = self.cmd_rx.try_recv() {
                self.pending_cmds.push(cmd);
            }

            // Coalesce seek bursts: keep only the newest seek until a non-seek command.
            self.coalesced_cmds.clear();
            let mut pending_seek: Option<f64> = None;
            for cmd in self.pending_cmds.drain(..) {
                match cmd {
                    PlayerCommand::Seek(time) => {
                        pending_seek = Some(time);
                    }
                    other => {
                        if let Some(time) = pending_seek.take() {
                            self.coalesced_cmds.push(PlayerCommand::Seek(time));
                        }
                        self.coalesced_cmds.push(other);
                    }
                }
            }
            if let Some(time) = pending_seek.take() {
                self.coalesced_cmds.push(PlayerCommand::Seek(time));
            }

            // Take ownership of coalesced_cmds for iteration without borrowing self
            let cmds: Vec<PlayerCommand> = self.coalesced_cmds.drain(..).collect();
            for cmd in cmds {
                self.handle_command(cmd);
            }

            // Poll exclusive WASAPI events
            #[cfg(target_os = "windows")]
            self.poll_exclusive_events();

            // Poll playback state (shared/rodio mode)
            self.poll_playback();

            // Wait for next command or poll timeout (replaces sleep for lower seek latency)
            if let Ok(cmd) = self.cmd_rx.recv_timeout(Duration::from_millis(250)) {
                self.pending_cmds.push(cmd);
                // Drain any additional queued commands
                while let Ok(cmd) = self.cmd_rx.try_recv() {
                    self.pending_cmds.push(cmd);
                }
            }
        }
    }

    fn handle_command(&mut self, cmd: PlayerCommand) {
        match cmd {
            PlayerCommand::LoadData(data, load_gen, event_seq, track_id, resume_policy) => {
                self.handle_load_data(data, load_gen, event_seq, track_id, resume_policy);
            }
            PlayerCommand::LoadStreaming(buffer, load_gen, event_seq, track_id, resume_policy) => {
                self.handle_load_streaming(buffer, load_gen, event_seq, track_id, resume_policy);
            }
            PlayerCommand::Play => self.handle_play(),
            PlayerCommand::Pause => self.handle_pause(),
            PlayerCommand::Stop(event_seq) => self.handle_stop(event_seq),
            PlayerCommand::Seek(time) => self.handle_seek(time),
            PlayerCommand::SetVolume(vol) => self.handle_set_volume(vol),
            PlayerCommand::GetAudioDevices(req_id) => self.handle_get_audio_devices(req_id),
            PlayerCommand::SetAudioDevice { id, exclusive } => {
                self.handle_set_audio_device(id, exclusive);
            }
        }
    }

    fn resolve_resume_policy(&self, resume_policy: ResumePolicy, track_id: &str) -> Option<f64> {
        match resume_policy {
            ResumePolicy::Disabled => {
                if self.allow_startup_auto_resume {
                    self.resume_store.get(track_id)
                } else {
                    None
                }
            }
            ResumePolicy::Auto => self.resume_store.get(track_id),
            ResumePolicy::Explicit(t) => {
                if t.is_finite() && t > RESUME_MIN_SECONDS {
                    Some(t)
                } else {
                    None
                }
            }
        }
    }

    fn handle_load_data(
        &mut self,
        data: Vec<u8>,
        load_gen: u32,
        event_seq: u32,
        track_id: String,
        resume_policy: ResumePolicy,
    ) {
        if load_gen != LOAD_SEQ.load(Relaxed) {
            crate::vprintln!("[LOAD #{load_gen}] stale LoadData, ignoring");
            return;
        }
        self.current_track_id = Some(track_id.clone());
        self.pending_resume_seek = self.resolve_resume_policy(resume_policy, &track_id);
        self.current_seq = event_seq;
        crate::state::GOVERNOR.reset_buffer_progress();

        #[cfg(target_os = "windows")]
        if let Some(cancel) = self.exclusive_stream_cancel.take() {
            cancel.store(true, Relaxed);
        }

        if let Some(ref old_buf) = self.current_streaming_buffer {
            old_buf.cancel();
        }
        self.current_streaming_buffer = None;

        let data = Arc::new(data);
        let decode_start = std::time::Instant::now();

        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                if let Some(ref handle) = self.exclusive_handle {
                    handle.send(ExclusiveCommand::Stop);
                    let cancel = Arc::new(AtomicBool::new(false));
                    self.exclusive_stream_cancel = Some(cancel.clone());

                    let cmd_tx = handle.command_sender();
                    let reader = SharedCursor::new(data.clone());
                    let total_len = data.len() as u64;
                    let stream_id = EXCLUSIVE_STREAM_SEQ.fetch_add(1, Relaxed) + 1;
                    thread::spawn(move || {
                        if let Err(e) = wasapi::stream_flac_reader_to_wasapi(
                            reader,
                            total_len,
                            stream_id,
                            cmd_tx,
                            cancel.clone(),
                        ) && !cancel.load(Relaxed)
                        {
                            eprintln!("[WASAPI] Progressive decode failed: {e}");
                        }
                    });

                    crate::vprintln!(
                        "[WASAPI] Progressive decode started ({:.0}ms setup)",
                        decode_start.elapsed().as_secs_f64() * 1000.0
                    );
                    self.current_data = Some(data);
                    self.current_streaming_buffer = None;
                    self.has_track = true;
                    self.is_playing = true;
                }
                return;
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
                if let Some(old) = self.rodio_player.take() {
                    old.stop();
                    drop(old);
                }

                let player = rodio::Player::connect_new(self.device_sink.mixer());
                player.set_volume(self.current_volume);
                player.append(source);
                player.pause();

                self.current_duration = source_duration;
                self.is_playing = false;
                self.has_track = true;
                self.last_empty = false;
                self.current_data = Some(data);
                self.rodio_player = Some(player);

                if self.current_duration > 0.0 {
                    (self.callback)(PlayerEvent::Duration(
                        self.current_duration,
                        self.current_seq,
                    ));
                }
                let initial_time = self.pending_resume_seek.unwrap_or(0.0);
                (self.callback)(PlayerEvent::TimeUpdate(initial_time, self.current_seq));

                let bitrate = if self.current_duration > 0.0 {
                    (byte_len as f64 * 8.0 / self.current_duration / 1000.0) as u32
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
                    format_duration_mmss(self.current_duration),
                    format_ms(decoder_ms),
                    format_ms(decode_start.elapsed().as_secs_f64() * 1000.0)
                );
            }
            Err(e) => {
                eprintln!("[ERROR]  Decode failed: {}", e);
            }
        }
    }

    fn handle_load_streaming(
        &mut self,
        buffer: StreamingBuffer,
        load_gen: u32,
        event_seq: u32,
        track_id: String,
        resume_policy: ResumePolicy,
    ) {
        if load_gen != LOAD_SEQ.load(Relaxed) {
            crate::vprintln!("[LOAD #{load_gen}] stale LoadStreaming, cancelling");
            buffer.cancel();
            return;
        }
        self.current_track_id = Some(track_id.clone());
        self.pending_resume_seek = self.resolve_resume_policy(resume_policy, &track_id);
        self.current_seq = event_seq;
        #[cfg(target_os = "windows")]
        if let Some(cancel) = self.exclusive_stream_cancel.take() {
            cancel.store(true, Relaxed);
        }

        if let Some(ref old_buf) = self.current_streaming_buffer {
            old_buf.cancel();
        }

        let decode_start = std::time::Instant::now();

        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                if let Some(ref handle) = self.exclusive_handle {
                    handle.send(ExclusiveCommand::Stop);

                    let cancel = Arc::new(AtomicBool::new(false));
                    self.exclusive_stream_cancel = Some(cancel.clone());

                    let cmd_tx = handle.command_sender();
                    let stream_reader = buffer.new_reader();
                    let total_len = buffer.total_len();
                    let stream_id = EXCLUSIVE_STREAM_SEQ.fetch_add(1, Relaxed) + 1;
                    thread::spawn(move || {
                        if let Err(e) = wasapi::stream_flac_reader_to_wasapi(
                            stream_reader,
                            total_len,
                            stream_id,
                            cmd_tx,
                            cancel.clone(),
                        ) && !cancel.load(Relaxed)
                        {
                            eprintln!("[WASAPI] Stream decode failed: {e}");
                        }
                    });

                    crate::vprintln!(
                        "[WASAPI] Progressive streaming decode started ({:.0}ms setup)",
                        decode_start.elapsed().as_secs_f64() * 1000.0
                    );
                    self.current_data = None;
                    self.current_streaming_buffer = Some(buffer);
                    self.has_track = true;
                    self.is_playing = true;
                } else {
                    buffer.cancel();
                }
                return;
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
                if let Some(old) = self.rodio_player.take() {
                    old.stop();
                    drop(old);
                }

                let player = rodio::Player::connect_new(self.device_sink.mixer());
                player.set_volume(self.current_volume);
                player.append(source);
                player.pause();

                self.current_duration = source_duration;
                self.is_playing = false;
                self.has_track = true;
                self.last_empty = false;
                self.current_data = None;
                self.current_streaming_buffer = Some(buffer);
                self.rodio_player = Some(player);

                if self.current_duration > 0.0 {
                    (self.callback)(PlayerEvent::Duration(
                        self.current_duration,
                        self.current_seq,
                    ));
                }
                let initial_time = self.pending_resume_seek.unwrap_or(0.0);
                (self.callback)(PlayerEvent::TimeUpdate(initial_time, self.current_seq));

                let bitrate = if self.current_duration > 0.0 {
                    (total_len as f64 * 8.0 / self.current_duration / 1000.0) as u32
                } else {
                    0
                };
                // Publish bitrate (bytes/sec) for governor hysteresis.
                let bitrate_bps = if self.current_duration > 0.0 {
                    (total_len as f64 / self.current_duration) as u64
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
                    format_duration_mmss(self.current_duration),
                    format_ms(decoder_ms),
                    format_ms(decode_start.elapsed().as_secs_f64() * 1000.0)
                );
            }
            Err(e) => {
                eprintln!("[ERROR]  Streaming decode failed: {}", e);
                buffer.cancel();
                self.current_streaming_buffer = None;
            }
        }
    }

    fn handle_play(&mut self) {
        self.allow_startup_auto_resume = false;
        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                if let Some(seek_time) = self.pending_resume_seek.take()
                    && let Some(ref handle) = self.exclusive_handle
                {
                    handle.send(ExclusiveCommand::Seek(seek_time));
                    (self.callback)(PlayerEvent::TimeUpdate(seek_time, self.current_seq));
                }
                if let Some(ref handle) = self.exclusive_handle {
                    handle.send(ExclusiveCommand::Play);
                }
                self.is_playing = true;
                crate::state::GOVERNOR
                    .buffer_progress()
                    .set_playback_active(true);
                return;
            }
        }
        if let Some(ref p) = self.rodio_player {
            let mut resumed_on_play: Option<f64> = None;
            if let Some(seek_time) = self.pending_resume_seek.take() {
                match p.try_seek(Duration::from_secs_f64(seek_time)) {
                    Ok(()) => {
                        if let Some(track_id) = self.current_track_id.as_ref() {
                            self.resume_store.set(track_id, seek_time);
                        }
                        resumed_on_play = Some(seek_time.max(0.0));
                        (self.callback)(PlayerEvent::TimeUpdate(
                            seek_time.max(0.0),
                            self.current_seq,
                        ));
                    }
                    Err(e) => {
                        // Retry on next Play if decoder wasn't ready yet.
                        self.pending_resume_seek = Some(seek_time);
                        eprintln!("[SEEK]   resume seek failed on play: {e}");
                    }
                }
            }
            p.play();
            let pos_secs = p.get_pos().as_secs_f64();
            // Right after a successful resume seek, some backends can briefly
            // still report 0; avoid overriding the UI resume position with 0.
            if resumed_on_play.is_none() || pos_secs > 0.0 {
                (self.callback)(PlayerEvent::TimeUpdate(pos_secs, self.current_seq));
            }
        }
        self.is_playing = true;
        crate::state::GOVERNOR
            .buffer_progress()
            .set_playback_active(true);
        (self.callback)(PlayerEvent::StateChange("active", self.current_seq));
    }

    fn handle_pause(&mut self) {
        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                if let Some(ref handle) = self.exclusive_handle {
                    handle.send(ExclusiveCommand::Pause);
                }
                self.is_playing = false;
                crate::state::GOVERNOR
                    .buffer_progress()
                    .set_playback_active(false);
                self.resume_store.flush_if_due(true);
                return;
            }
        }
        if let Some(ref p) = self.rodio_player {
            p.pause();
            let pos_secs = p.get_pos().as_secs_f64();
            (self.callback)(PlayerEvent::TimeUpdate(pos_secs, self.current_seq));
        }
        self.is_playing = false;
        crate::state::GOVERNOR
            .buffer_progress()
            .set_playback_active(false);
        (self.callback)(PlayerEvent::StateChange("paused", self.current_seq));
        self.resume_store.flush_if_due(true);
    }

    fn handle_stop(&mut self, event_seq: u32) {
        self.current_seq = event_seq;
        crate::state::GOVERNOR.reset_buffer_progress();

        #[cfg(target_os = "windows")]
        if let Some(cancel) = self.exclusive_stream_cancel.take() {
            cancel.store(true, Relaxed);
        }

        if let Some(ref old_buf) = self.current_streaming_buffer {
            old_buf.cancel();
        }
        self.current_streaming_buffer = None;

        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                if let Some(ref handle) = self.exclusive_handle {
                    handle.send(ExclusiveCommand::Stop);
                }
                (self.callback)(PlayerEvent::TimeUpdate(0.0, self.current_seq));
                (self.callback)(PlayerEvent::StateChange("paused", self.current_seq));
                self.is_playing = false;
                self.has_track = false;
                self.current_duration = 0.0;
                self.current_data = None;
                self.current_track_id = None;
                self.pending_resume_seek = None;
                self.resume_store.flush_if_due(true);

                return;
            }
        }
        if let Some(old) = self.rodio_player.take() {
            old.stop();
            drop(old);
        }
        (self.callback)(PlayerEvent::TimeUpdate(0.0, self.current_seq));
        (self.callback)(PlayerEvent::StateChange("paused", self.current_seq));
        self.is_playing = false;
        self.has_track = false;
        self.current_duration = 0.0;
        self.current_data = None;
        self.current_track_id = None;
        self.pending_resume_seek = None;
        self.resume_store.flush_if_due(true);
    }

    fn handle_seek(&mut self, time: f64) {
        // Latest-seek-wins: drop stale seeks queued while we were busy.
        let mut latest_time = time;
        while let Ok(next_cmd) = self.cmd_rx.try_recv() {
            match next_cmd {
                PlayerCommand::Seek(t) => {
                    latest_time = t;
                }
                other => self.pending_cmds.push(other),
            }
        }
        if let Some(track_id) = self.current_track_id.as_ref() {
            self.resume_store.set(track_id, latest_time);
            self.resume_store.flush_if_due(false);
        }
        self.pending_resume_seek = None;

        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                if self.has_track {
                    if let Some(ref handle) = self.exclusive_handle {
                        handle.send(ExclusiveCommand::Seek(latest_time));
                    }
                    (self.callback)(PlayerEvent::TimeUpdate(
                        latest_time.max(0.0),
                        self.current_seq,
                    ));
                } else {
                    self.pending_resume_seek = Some(latest_time);
                }
                return;
            }
        }
        if let Some(ref p) = self.rodio_player {
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
                    crate::vprintln!("[SEEK]   try_seek OK: {timing}");
                    (self.callback)(PlayerEvent::TimeUpdate(
                        latest_time.max(0.0),
                        self.current_seq,
                    ));
                }
                Err(e) => {
                    eprintln!("[SEEK]   try_seek FAILED ({timing}): {e}");
                    self.pending_resume_seek = Some(latest_time);
                }
            }
        } else {
            self.pending_resume_seek = Some(latest_time);
            crate::vprintln!("[SEEK]   queued until player ready");
        }
    }

    fn handle_set_volume(&mut self, vol: f64) {
        self.current_volume = (vol / 100.0) as f32;
        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                // Volume ignored in exclusive mode (100% forced)
                return;
            }
        }
        if let Some(ref p) = self.rodio_player {
            p.set_volume(self.current_volume);
        }
    }

    fn handle_get_audio_devices(&self, req_id: Option<String>) {
        let devices = enumerate_audio_devices();
        (self.callback)(PlayerEvent::AudioDevices(devices, req_id));
    }

    fn handle_set_audio_device(&mut self, id: String, exclusive: bool) {
        let _ = &exclusive; // used on Windows only
        #[cfg(target_os = "windows")]
        {
            if let Some(cancel) = self.exclusive_stream_cancel.take() {
                cancel.store(true, Relaxed);
            }
            if exclusive {
                // Switch to exclusive WASAPI mode
                // Stop rodio playback
                if let Some(ref p) = self.rodio_player {
                    p.stop();
                }
                self.rodio_player = None;

                // Shutdown previous exclusive handle if any
                if let Some(old) = self.exclusive_handle.take() {
                    old.shutdown();
                }

                let handle = ExclusiveHandle::spawn(id.clone());
                self.is_exclusive_mode = true;
                self.exclusive_handle = Some(handle);

                // Reload current track in exclusive mode.
                if let Some(ref handle) = self.exclusive_handle {
                    if let Some(ref streaming_buf) = self.current_streaming_buffer {
                        handle.send(ExclusiveCommand::Stop);
                        let cancel = Arc::new(AtomicBool::new(false));
                        self.exclusive_stream_cancel = Some(cancel.clone());

                        let cmd_tx = handle.command_sender();
                        let reader = streaming_buf.new_reader();
                        let total_len = streaming_buf.total_len();
                        let stream_id = EXCLUSIVE_STREAM_SEQ.fetch_add(1, Relaxed) + 1;
                        thread::spawn(move || {
                            if let Err(e) = wasapi::stream_flac_reader_to_wasapi(
                                reader,
                                total_len,
                                stream_id,
                                cmd_tx,
                                cancel.clone(),
                            ) && !cancel.load(Relaxed)
                            {
                                eprintln!("[WASAPI] Device switch stream decode failed: {e}");
                            }
                        });
                        self.has_track = true;
                        self.is_playing = true;
                    } else if let Some(ref data) = self.current_data {
                        handle.send(ExclusiveCommand::Stop);
                        let cancel = Arc::new(AtomicBool::new(false));
                        self.exclusive_stream_cancel = Some(cancel.clone());

                        let cmd_tx = handle.command_sender();
                        let reader = SharedCursor::new(data.clone());
                        let total_len = data.len() as u64;
                        let stream_id = EXCLUSIVE_STREAM_SEQ.fetch_add(1, Relaxed) + 1;
                        thread::spawn(move || {
                            if let Err(e) = wasapi::stream_flac_reader_to_wasapi(
                                reader,
                                total_len,
                                stream_id,
                                cmd_tx,
                                cancel.clone(),
                            ) && !cancel.load(Relaxed)
                            {
                                eprintln!("[WASAPI] Device switch stream decode failed: {e}");
                            }
                        });
                        self.has_track = true;
                        self.is_playing = true;
                    }
                }

                crate::vprintln!("[AUDIO] Switched to exclusive WASAPI: {}", id);
                return;
            } else if self.is_exclusive_mode {
                // Switch back from exclusive to shared mode
                if let Some(old) = self.exclusive_handle.take() {
                    old.shutdown();
                }
                self.is_exclusive_mode = false;
                crate::vprintln!("[AUDIO] Switched back to shared mode");
                // Fall through to shared device setup below
            }
        }

        let position = self.rodio_player.as_ref().map(|p| p.get_pos());
        let was_playing = self.is_playing;

        match open_device_sink(Some(&id)) {
            Ok(mut new_sink) => {
                new_sink.log_on_drop(false);

                // Stop old player
                if let Some(ref p) = self.rodio_player {
                    p.stop();
                }

                self.device_sink = new_sink;

                // Reload current track on new device
                if let Some(ref streaming_buf) = self.current_streaming_buffer {
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
                                let player = rodio::Player::connect_new(self.device_sink.mixer());
                                player.set_volume(self.current_volume);
                                player.append(source);

                                if let Some(pos) = position {
                                    let _ = player.try_seek(pos);
                                }
                                if !was_playing {
                                    player.pause();
                                }

                                self.current_data = Some(data);
                                self.current_streaming_buffer = None;
                                self.rodio_player = Some(player);
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
                            let player = rodio::Player::connect_new(self.device_sink.mixer());
                            player.set_volume(self.current_volume);
                            player.append(source);

                            if let Some(pos) = position {
                                let _ = player.try_seek(pos);
                            }
                            if !was_playing {
                                player.pause();
                            }

                            self.rodio_player = Some(player);
                        }
                    }
                } else if let Some(ref data) = self.current_data {
                    let byte_len = data.len() as u64;
                    let cursor = SharedCursor::new(data.clone());
                    if let Ok(source) = Decoder::builder()
                        .with_data(cursor)
                        .with_byte_len(byte_len)
                        .with_seekable(true)
                        .with_hint("flac")
                        .build()
                    {
                        let player = rodio::Player::connect_new(self.device_sink.mixer());
                        player.set_volume(self.current_volume);
                        player.append(source);

                        if let Some(pos) = position {
                            let _ = player.try_seek(pos);
                        }

                        if !was_playing {
                            player.pause();
                        }

                        self.rodio_player = Some(player);
                    }
                }

                crate::vprintln!("[AUDIO] Switched to device: {}", id);
            }
            Err(e) => {
                eprintln!("[ERROR] Failed to switch to device '{}': {}", id, e);
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn poll_exclusive_events(&mut self) {
        if self.is_exclusive_mode {
            if let Some(ref handle) = self.exclusive_handle {
                for ev in handle.poll_events() {
                    match ev {
                        ExclusiveEvent::TimeUpdate(t) => {
                            if let Some(track_id) = self.current_track_id.as_ref() {
                                self.resume_store.set(track_id, t);
                                self.resume_store.flush_if_due(false);
                            }
                            (self.callback)(PlayerEvent::TimeUpdate(t, self.current_seq));
                        }
                        ExclusiveEvent::StateChange(s) => {
                            if s == "completed" {
                                if let Some(track_id) = self.current_track_id.as_ref() {
                                    self.resume_store.clear(track_id);
                                    self.resume_store.flush_if_due(true);
                                }
                                self.has_track = false;
                                self.is_playing = false;
                                self.current_track_id = None;
                                crate::state::GOVERNOR
                                    .buffer_progress()
                                    .set_playback_active(false);
                            }
                            (self.callback)(PlayerEvent::StateChange(s, self.current_seq));
                        }
                        ExclusiveEvent::Duration(d) => {
                            self.current_duration = d;
                            (self.callback)(PlayerEvent::Duration(d, self.current_seq));
                            if let Some(seek_time) = self.pending_resume_seek.take() {
                                handle.send(ExclusiveCommand::Seek(seek_time));
                                (self.callback)(PlayerEvent::TimeUpdate(
                                    seek_time,
                                    self.current_seq,
                                ));
                            }
                        }
                        ExclusiveEvent::InitFailed(e) => {
                            eprintln!("[WASAPI] Init failed, falling back to shared mode: {e}");
                            if let Some(cancel) = self.exclusive_stream_cancel.take() {
                                cancel.store(true, Relaxed);
                            }
                            self.is_exclusive_mode = false;
                            // Handle will be dropped
                        }
                        ExclusiveEvent::Stopped => {
                            self.has_track = false;
                            self.is_playing = false;
                            self.current_track_id = None;
                            crate::state::GOVERNOR
                                .buffer_progress()
                                .set_playback_active(false);
                            self.resume_store.flush_if_due(true);
                        }
                    }
                }
            }

            // If we got InitFailed, drop the handle
            if !self.is_exclusive_mode {
                self.exclusive_handle = None;
            }
        }
    }

    fn poll_playback(&mut self) {
        #[cfg(target_os = "windows")]
        let should_poll_rodio = !self.is_exclusive_mode;
        #[cfg(not(target_os = "windows"))]
        let should_poll_rodio = true;

        if should_poll_rodio
            && self.has_track
            && self.is_playing
            && let Some(ref p) = self.rodio_player
        {
            let pos = p.get_pos();
            let pos_secs = pos.as_secs_f64();

            if pos_secs > 0.0 {
                if let Some(track_id) = self.current_track_id.as_ref() {
                    self.resume_store.set(track_id, pos_secs);
                    self.resume_store.flush_if_due(false);
                }
                (self.callback)(PlayerEvent::TimeUpdate(pos_secs, self.current_seq));
            }

            if p.empty() && !self.last_empty {
                if let Some(track_id) = self.current_track_id.as_ref() {
                    self.resume_store.clear(track_id);
                    self.resume_store.flush_if_due(true);
                }
                (self.callback)(PlayerEvent::TimeUpdate(
                    self.current_duration,
                    self.current_seq,
                ));
                (self.callback)(PlayerEvent::StateChange("completed", self.current_seq));
                self.has_track = false;
                self.is_playing = false;
                self.current_track_id = None;
                crate::state::GOVERNOR
                    .buffer_progress()
                    .set_playback_active(false);
                self.current_duration = 0.0;
                self.last_empty = true;
            }
        }
    }
}
