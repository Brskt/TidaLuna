mod commands;
pub(super) mod decode;
mod device;
pub(super) mod output;
mod playback;

use super::buffer::RamBuffer;
use super::resume::ResumeStore;
use super::{PlayerCommand, PlayerEvent};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64};
use std::sync::mpsc;
use std::time::Duration;

#[cfg(target_os = "windows")]
use super::wasapi;
#[cfg(target_os = "windows")]
use wasapi::ExclusiveHandle;

/// Maximum difference (seconds) between a pre-seeked position and a seek
/// target for the pre-seek to be considered "close enough" to skip.
const PRE_SEEK_TOLERANCE: f64 = 2.0;

enum DecodeCommand {
    Seek(f64),
    Pause,
    Resume,
    Stop,
}

enum DecodeEvent {
    /// Track decoded to completion (EOF).
    Finished,
    /// Decode error (non-fatal, logged).
    Error(String),
    /// Seek completed — audio output can resume.
    SeekComplete,
}

pub(super) struct PlayerThread<F> {
    cmd_rx: mpsc::Receiver<PlayerCommand>,
    callback: F,
    // Audio output
    cpal_stream: Option<cpal::Stream>,
    volume: Arc<AtomicU32>, // f32 bits stored as u32
    // Decode thread
    decode_cmd_tx: Option<mpsc::Sender<DecodeCommand>>,
    decode_event_rx: Option<mpsc::Receiver<DecodeEvent>>,
    decode_handle: Option<std::thread::JoinHandle<()>>,
    // Track state
    current_buffer: Option<RamBuffer>,
    current_track_id: Option<String>,
    current_format: String,
    is_cached: bool,
    is_playing: bool,
    has_track: bool,
    current_duration: f64,
    current_seq: u32,
    // Position tracking
    decoded_samples: Arc<AtomicU64>,
    sample_rate: u32,
    channels: u16,
    // Resume
    resume_store: ResumeStore,
    pending_resume_seek: Option<f64>,
    pre_seek_pos: Option<f64>,
    allow_startup_auto_resume: bool,
    // Device
    current_device_id: Option<String>,
    // WASAPI exclusive
    #[cfg(target_os = "windows")]
    exclusive_handle: Option<ExclusiveHandle>,
    #[cfg(target_os = "windows")]
    is_exclusive_mode: bool,
    #[cfg(target_os = "windows")]
    exclusive_stream_cancel: Option<Arc<AtomicBool>>,
    // Seek state
    seeking: bool,
    seek_target: Option<f64>,
    seek_wall_start: Option<std::time::Instant>,
    cpal_muted: Option<Arc<AtomicBool>>,
    cpal_stream_error: Option<Arc<AtomicU8>>,
    played_samples: Arc<AtomicU64>,
    // Buffering state
    buffer_stalled: bool,
    pending_complete: bool,
    last_played_snapshot: u64,
    version_emitted: bool,
    // Command coalescing
    pending_cmds: Vec<PlayerCommand>,
    coalesced_cmds: Vec<PlayerCommand>,
}

impl<F: Fn(PlayerEvent) + Send + 'static> PlayerThread<F> {
    pub fn new(cmd_rx: mpsc::Receiver<PlayerCommand>, callback: F) -> Option<Self> {
        Some(Self {
            cmd_rx,
            callback,
            cpal_stream: None,
            volume: Arc::new(AtomicU32::new(f32::to_bits(1.0))),
            decode_cmd_tx: None,
            decode_event_rx: None,
            decode_handle: None,
            current_buffer: None,
            current_track_id: None,
            current_format: String::new(),
            is_cached: false,
            is_playing: false,
            has_track: false,
            current_duration: 0.0,
            current_seq: 0,
            decoded_samples: Arc::new(AtomicU64::new(0)),
            sample_rate: 44100,
            channels: 2,
            resume_store: ResumeStore::load(),
            pending_resume_seek: None,
            pre_seek_pos: None,
            allow_startup_auto_resume: true,
            current_device_id: None,
            #[cfg(target_os = "windows")]
            exclusive_handle: None,
            #[cfg(target_os = "windows")]
            is_exclusive_mode: false,
            #[cfg(target_os = "windows")]
            exclusive_stream_cancel: None,
            seeking: false,
            seek_target: None,
            seek_wall_start: None,
            cpal_muted: None,
            cpal_stream_error: None,
            played_samples: Arc::new(AtomicU64::new(0)),
            buffer_stalled: false,
            pending_complete: false,
            last_played_snapshot: 0,
            version_emitted: false,
            pending_cmds: Vec::new(),
            coalesced_cmds: Vec::new(),
        })
    }

    pub fn run(&mut self) {
        loop {
            while let Ok(cmd) = self.cmd_rx.try_recv() {
                self.pending_cmds.push(cmd);
            }

            // Coalesce seek bursts
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

            let cmds: Vec<PlayerCommand> = self.coalesced_cmds.drain(..).collect();
            for cmd in cmds {
                self.handle_command(cmd);
            }

            // Poll exclusive WASAPI events
            #[cfg(target_os = "windows")]
            self.poll_exclusive_events();

            // Poll playback state
            self.poll_playback();

            // Wait for next command
            let timeout = if self.seeking {
                Duration::from_millis(1)
            } else {
                Duration::from_millis(250)
            };
            if let Ok(cmd) = self.cmd_rx.recv_timeout(timeout) {
                self.pending_cmds.push(cmd);
                while let Ok(cmd) = self.cmd_rx.try_recv() {
                    self.pending_cmds.push(cmd);
                }
            }
        }
    }

    fn handle_command(&mut self, cmd: PlayerCommand) {
        match cmd {
            PlayerCommand::Load { request, auto_play } => {
                self.handle_load(request);
                if auto_play {
                    crate::vprintln!("[AUTO]   Auto-play after load");
                    self.handle_play();
                }
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
            PlayerCommand::EmitMediaError { error, code } => {
                (self.callback)(PlayerEvent::MediaError { error, code });
            }
            PlayerCommand::EmitMaxConnections => {
                (self.callback)(PlayerEvent::MaxConnectionsReached);
            }
        }
    }
}
