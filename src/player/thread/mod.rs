mod commands;
pub(super) mod decode;
mod device;
pub(super) mod output;
mod playback;

use super::buffer::RamBuffer;
use super::resume::ResumeStore;
use super::{LOAD_SEQ, MediaFormatSnapshot, PlayerCommand, PlayerEvent};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64};
use std::sync::mpsc;
use std::time::Duration;

#[cfg(target_os = "windows")]
use super::asio::host::AsioHandle;
#[cfg(target_os = "windows")]
use super::wasapi;
#[cfg(target_os = "windows")]
use wasapi::ExclusiveHandle;

/// Maximum difference (seconds) between a pre-seeked position and a seek
/// target for the pre-seek to be considered "close enough" to skip.
const PRE_SEEK_TOLERANCE: f64 = 2.0;

/// How long exclusive output stays paused before the device is released back to
/// other apps. A short pause/resume within this window keeps the device (instant
/// resume, no DAC pop); a sustained pause frees it. TIDAL has no stop button, so
/// pause is the "stopped listening" signal.
#[cfg(target_os = "windows")]
const EXCLUSIVE_PAUSE_RELEASE: std::time::Duration = std::time::Duration::from_secs(10);

/// How long ASIO stays idle (paused or terminally stopped) before the driver is
/// shut down so other apps regain the device. A short pause/resume within the
/// window keeps the driver (instant resume, no pop); a resume after the release
/// respawns it.
#[cfg(target_os = "windows")]
const ASIO_IDLE_RELEASE: std::time::Duration = std::time::Duration::from_secs(10);

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
    /// Seek completed - audio output can resume.
    SeekComplete,
}

pub(super) struct PlayerThread<F> {
    cmd_rx: mpsc::Receiver<PlayerCommand>,
    callback: F,
    // Audio output
    cpal_stream: Option<cpal::Stream>,
    volume: Arc<AtomicU32>, // f32 bits stored as u32
    // The app's authoritative volume (f32 bits): written by handle_set_volume on
    // every user/echoed change, adopted from the session only at cold start (see
    // volume_baseline_established). The sole seed source for the exclusive/ASIO
    // digital gain -- never a live GetMasterVolume() query, which can be a fresh 1.0.
    #[cfg(target_os = "windows")]
    last_volume: Arc<AtomicU32>,
    // Digital gain for the exclusive WASAPI render (it bypasses the OS mixer).
    #[cfg(target_os = "windows")]
    exclusive_gain: Arc<AtomicU32>,
    // True once a session-volume baseline has been adopted (cold start). After that
    // we own the volume: re-inits assert last_volume into the (possibly fresh)
    // session via SetMasterVolume instead of adopting its default-1.0 GetMasterVolume.
    #[cfg(target_os = "windows")]
    volume_baseline_established: bool,
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
    // Whether a same-track re-assert (ReassertResume) should resume: handle_stop
    // sets it to the pre-stop is_playing, a user pause clears it (handle_pause).
    // So a user-paused track stays paused on re-assert; a quality-swap's stop->load->play
    // (was playing) still resumes. A re-assert whose load carries the user's
    // play-intent resumes regardless (ReassertResume { want_play }).
    resume_on_reassert: bool,
    // A play that arrived before the track finished loading, tagged with the
    // LOAD_SEQ generation it was meant for. Applied once that load reaches the
    // ready state so a play racing ahead of the async load isn't dropped, and
    // never applied to a track the user has since skipped past.
    pending_play: Option<u32>,
    // Generation of the in-flight load (set by LoadStarted, cleared by
    // LoadSettled/handle_stop). The signal that a no-track play should defer
    // (a load is coming) rather than re-arm the retained source.
    loading_gen: Option<u32>,
    current_duration: f64,
    // Format of the committed track as last emitted (None for ASIO/exclusive
    // loads, which skip the shared probe); re-sent on ReassertResume.
    last_media_format: Option<MediaFormatSnapshot>,
    current_seq: u32,
    // Position tracking
    decoded_samples: Arc<AtomicU64>,
    sample_rate: u32,
    channels: u16,
    // Resume
    resume_store: ResumeStore,
    pending_resume_seek: Option<f64>,
    // A user seek that arrived before the exclusive stream was ready. Supersedes
    // pending_resume_seek at exclusive start (explicit seek wins over auto-resume).
    #[cfg(target_os = "windows")]
    user_seek_override: Option<f64>,
    pre_seek_pos: Option<f64>,
    allow_startup_auto_resume: bool,
    // Device
    current_device_id: Option<String>,
    // Concrete name of the open cpal device, so the shared re-assert guard
    // compares physical identity, not the raw id the load path never stores.
    current_output_name: Option<String>,
    // Did the open stream come from a default selector ("auto"/"default"/none)?
    // On Windows it follows the OS default; a named stream is pinned.
    output_is_default: bool,
    // WASAPI exclusive
    #[cfg(target_os = "windows")]
    exclusive_handle: Option<ExclusiveHandle>,
    #[cfg(target_os = "windows")]
    is_exclusive_mode: bool,
    #[cfg(target_os = "windows")]
    exclusive_stream_cancel: Option<Arc<AtomicBool>>,
    // Sender to the live exclusive decoder for in-place seeks (no respawn).
    #[cfg(target_os = "windows")]
    exclusive_seek_tx: Option<mpsc::Sender<f64>>,
    // Live exclusive stream id: scopes Play/Pause to the adopted stream (mirrors
    // current_asio_stream_id), so a premature or stale command can't act on the
    // old render context.
    #[cfg(target_os = "windows")]
    current_exclusive_stream_id: Option<u32>,
    // Live exclusive position (floor-free, this session): restores the position
    // on an exclusive->shared switch, where resume_store's 1s floor and
    // cross-session persistence would lose it or return a stale offset.
    #[cfg(target_os = "windows")]
    last_exclusive_pos: Option<f64>,
    // Debounced device release: armed (now + EXCLUSIVE_PAUSE_RELEASE) when exclusive
    // playback pauses; if it elapses still-paused, the IAudioClient is dropped so other
    // apps regain the device. Cleared on resume/load/re-engage, so a short pause or a
    // track-change stop->load within the window never releases.
    #[cfg(target_os = "windows")]
    exclusive_release_at: Option<std::time::Instant>,
    // ASIO output (parallel to the exclusive fields; mutually exclusive with it).
    #[cfg(target_os = "windows")]
    asio_handle: Option<AsioHandle>,
    // Debounced driver release: armed by a sustained pause or a terminal stop,
    // consumed in poll_asio_events once it elapses with no load/play in between.
    // The next ASIO load respawns the handle (start_asio_playback self-heal).
    #[cfg(target_os = "windows")]
    asio_release_at: Option<std::time::Instant>,
    // Parked ASIO teardown: the control thread drains ASIOStop/Release
    // driver-side; joining froze the command thread. Polled by
    // poll_asio_teardown; gates any ASIO respawn (one driver instance at a time).
    #[cfg(target_os = "windows")]
    asio_teardown: Option<std::thread::JoinHandle<()>>,
    // Device switch parked behind asio_teardown (latest wins), re-dispatched
    // once the teardown drains.
    #[cfg(target_os = "windows")]
    pending_device_switch: Option<(String, super::OutputMode)>,
    #[cfg(target_os = "windows")]
    is_asio_mode: bool,
    #[cfg(target_os = "windows")]
    asio_stream_cancel: Option<Arc<AtomicBool>>,
    // Sender to the live ASIO decoder for in-place seeks (no respawn).
    #[cfg(target_os = "windows")]
    asio_seek_tx: Option<mpsc::Sender<f64>>,
    // The live ASIO decoder's stream_id; gates stale Stopped/Completed events (from a
    // superseded stream) so they can't null a newer track's has_track.
    #[cfg(target_os = "windows")]
    current_asio_stream_id: Option<u32>,
    // Live ASIO position (floor-free, this session): restores the position on an
    // asio->shared switch (same rationale as last_exclusive_pos).
    #[cfg(target_os = "windows")]
    last_asio_pos: Option<f64>,
    // ASIO progress watchdog: anchor reset on Active and on each forward TimeUpdate. If the
    // clock is started+playing but the position never advances within the timeout, a genuine
    // driver failure not recovered by the asio_message reset path -> fall back to shared so the
    // track still plays (resampled).
    #[cfg(target_os = "windows")]
    asio_watchdog_at: Option<std::time::Instant>,
    #[cfg(target_os = "windows")]
    asio_watchdog_pos: f64,
    // Per-track ASIO skip: the canonical id of a track whose sample rate the device can't clock
    // (RateUnsupported). Its shared re-arm must NOT re-engage ASIO (loop); cleared when a
    // different track loads, so other rates keep using ASIO. Does NOT disable ASIO globally.
    #[cfg(target_os = "windows")]
    asio_skip_track: Option<String>,
    // Per-track exclusive skip: same idea for a track whose format the device can't do in
    // exclusive (FormatUnsupported); its re-arm plays shared without re-engaging exclusive,
    // while exclusive stays enabled for other rates. Does NOT disable exclusive globally.
    #[cfg(target_os = "windows")]
    exclusive_skip_track: Option<String>,
    // Volume sync (Windows session volume)
    #[cfg(target_os = "windows")]
    _com_guard: Option<crate::platform::volume_sync::ComGuard>,
    #[cfg(target_os = "windows")]
    volume_sync: Option<crate::platform::volume_sync::VolumeSync>,
    #[cfg(target_os = "windows")]
    volume_rx: Option<mpsc::Receiver<f64>>,
    #[cfg(target_os = "windows")]
    volume_sync_enabled: bool,
    #[cfg(target_os = "windows")]
    pending_unmute: bool,
    // Seek state
    seeking: bool,
    seek_target: Option<f64>,
    last_seek_emit: Option<f64>,
    seek_wall_start: Option<std::time::Instant>,
    cpal_muted: Option<Arc<AtomicBool>>,
    cpal_mute_ack: Option<Arc<AtomicBool>>,
    cpal_stream_error: Option<Arc<AtomicU8>>,
    played_samples: Arc<AtomicU64>,
    // Buffering state
    buffer_stalled: bool,
    pending_complete: bool,
    // Shared with `Player` (IPC thread): the committed track as
    // `(canonical_id, format)`, or `None` when nothing is loaded. Set in
    // `handle_load`, cleared on every track-end path. Lets `Player::load` resume
    // a same-track re-assert instead of rebuilding (the reconcile idempotency
    // signal), keyed on the committed track rather than the requested one.
    committed_track: Arc<std::sync::Mutex<Option<(String, String)>>>,
    last_played_snapshot: u64,
    version_emitted: bool,
    // Command coalescing
    pending_cmds: Vec<PlayerCommand>,
    coalesced_cmds: Vec<PlayerCommand>,
}

impl<F: Fn(PlayerEvent) + Send + 'static> PlayerThread<F> {
    pub fn new(
        cmd_rx: mpsc::Receiver<PlayerCommand>,
        callback: F,
        #[allow(unused_variables)] volume_sync_enabled: bool,
        committed_track: Arc<std::sync::Mutex<Option<(String, String)>>>,
    ) -> Option<Self> {
        #[cfg(target_os = "windows")]
        let com_guard = match crate::platform::volume_sync::ComGuard::new() {
            Ok(g) => Some(g),
            Err(e) => {
                crate::vprintln!("[VOLUME] COM init failed: {e}, volume sync disabled");
                None
            }
        };

        Some(Self {
            cmd_rx,
            callback,
            cpal_stream: None,
            volume: Arc::new(AtomicU32::new(f32::to_bits(1.0))),
            #[cfg(target_os = "windows")]
            last_volume: Arc::new(AtomicU32::new(f32::to_bits(1.0))),
            #[cfg(target_os = "windows")]
            exclusive_gain: Arc::new(AtomicU32::new(f32::to_bits(1.0))),
            #[cfg(target_os = "windows")]
            volume_baseline_established: false,
            decode_cmd_tx: None,
            decode_event_rx: None,
            decode_handle: None,
            current_buffer: None,
            current_track_id: None,
            current_format: String::new(),
            is_cached: false,
            is_playing: false,
            has_track: false,
            resume_on_reassert: false,
            pending_play: None,
            loading_gen: None,
            current_duration: 0.0,
            last_media_format: None,
            current_seq: 0,
            decoded_samples: Arc::new(AtomicU64::new(0)),
            sample_rate: 44100,
            channels: 2,
            resume_store: ResumeStore::load(),
            pending_resume_seek: None,
            #[cfg(target_os = "windows")]
            user_seek_override: None,
            pre_seek_pos: None,
            allow_startup_auto_resume: true,
            current_device_id: None,
            current_output_name: None,
            output_is_default: false,
            #[cfg(target_os = "windows")]
            exclusive_handle: None,
            #[cfg(target_os = "windows")]
            is_exclusive_mode: false,
            #[cfg(target_os = "windows")]
            exclusive_stream_cancel: None,
            #[cfg(target_os = "windows")]
            exclusive_seek_tx: None,
            #[cfg(target_os = "windows")]
            current_exclusive_stream_id: None,
            #[cfg(target_os = "windows")]
            last_exclusive_pos: None,
            #[cfg(target_os = "windows")]
            exclusive_release_at: None,
            #[cfg(target_os = "windows")]
            asio_handle: None,
            #[cfg(target_os = "windows")]
            asio_release_at: None,
            #[cfg(target_os = "windows")]
            asio_teardown: None,
            #[cfg(target_os = "windows")]
            pending_device_switch: None,
            #[cfg(target_os = "windows")]
            is_asio_mode: false,
            #[cfg(target_os = "windows")]
            asio_stream_cancel: None,
            #[cfg(target_os = "windows")]
            asio_seek_tx: None,
            #[cfg(target_os = "windows")]
            current_asio_stream_id: None,
            #[cfg(target_os = "windows")]
            last_asio_pos: None,
            #[cfg(target_os = "windows")]
            asio_watchdog_at: None,
            #[cfg(target_os = "windows")]
            asio_watchdog_pos: 0.0,
            #[cfg(target_os = "windows")]
            asio_skip_track: None,
            #[cfg(target_os = "windows")]
            exclusive_skip_track: None,
            #[cfg(target_os = "windows")]
            _com_guard: com_guard,
            #[cfg(target_os = "windows")]
            volume_sync: None,
            #[cfg(target_os = "windows")]
            volume_rx: None,
            #[cfg(target_os = "windows")]
            volume_sync_enabled,
            #[cfg(target_os = "windows")]
            pending_unmute: false,
            seeking: false,
            seek_target: None,
            last_seek_emit: None,
            seek_wall_start: None,
            cpal_muted: None,
            cpal_mute_ack: None,
            cpal_stream_error: None,
            played_samples: Arc::new(AtomicU64::new(0)),
            buffer_stalled: false,
            pending_complete: false,
            committed_track,
            last_played_snapshot: 0,
            version_emitted: false,
            pending_cmds: Vec::new(),
            coalesced_cmds: Vec::new(),
        })
    }

    /// Update the committed-track reconcile signal shared with the IPC thread:
    /// `Some((canonical_id, format))` on commit, `None` on every track-end path.
    pub(super) fn set_committed_track(&self, track: Option<(String, String)>) {
        *self
            .committed_track
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = track;
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

            // Poll ASIO events
            #[cfg(target_os = "windows")]
            self.poll_asio_events();

            // Reap a drained ASIO teardown and run any parked device switch
            #[cfg(target_os = "windows")]
            self.poll_asio_teardown();

            #[cfg(target_os = "windows")]
            if let Some(ref rx) = self.volume_rx {
                let mut last = None;
                while let Ok(v) = rx.try_recv() {
                    last = Some(v);
                }
                if let Some(v) = last {
                    (self.callback)(PlayerEvent::VolumeSync(v));
                }
            }

            #[cfg(target_os = "windows")]
            if self.pending_unmute
                && let Some(ref ack) = self.cpal_mute_ack
                && ack.load(Relaxed)
            {
                ack.store(false, Relaxed);
                if let Some(ref muted) = self.cpal_muted {
                    muted.store(false, Relaxed);
                }
                self.pending_unmute = false;
            }

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
                // Auto-play only if the load delivered a live track: a stale load
                // that handle_load rejects must not reach handle_play (it would
                // re-arm and replay after a Stop).
                let loaded = self.handle_load(request, auto_play);
                if auto_play && loaded {
                    crate::vprintln!("[AUTO]   Auto-play after load");
                    self.handle_play();
                }
            }
            PlayerCommand::LoadStarted { generation } => self.handle_load_started(generation),
            PlayerCommand::LoadSettled { generation } => self.handle_load_settled(generation),
            PlayerCommand::Play => self.handle_play(),
            PlayerCommand::ReassertResume { want_play } => {
                // Same-track re-assert (boombox does stop()+load(same)): its load() awaits
                // `mediaduration` before the instance is seekable, but the idempotent skip
                // never re-runs handle_load, so that event is never re-emitted and boombox's
                // load() stalls, dropping later progress-bar seeks. Re-emit Duration (at the
                // committed track's seq) so the re-load resolves and stays seekable.
                if self.current_duration > 0.0 {
                    (self.callback)(PlayerEvent::Duration(
                        self.current_duration,
                        self.current_seq,
                    ));
                }
                // Same re-drive for the format snapshot the renderer nulls on every
                // forwarded load (None on ASIO/exclusive-probed tracks -> no stale send).
                if let Some(fmt) = self.last_media_format {
                    (self.callback)(fmt.to_event());
                }
                // Resume if the track was playing pre-stop (resume_on_reassert) or the
                // load carried the user's play-intent (want_play); the resume then drives
                // StateChange(Active), resolving boombox's post-duration active await so
                // it applies the assetPosition and completes.
                if self.resume_on_reassert || want_play {
                    self.handle_play();
                }
            }
            PlayerCommand::Pause => self.handle_pause(),
            PlayerCommand::Stop(event_seq) => self.handle_stop(event_seq),
            PlayerCommand::Seek(time) => self.handle_seek(time),
            PlayerCommand::SetVolume(vol) => self.handle_set_volume(vol),
            PlayerCommand::GetAudioDevices(req_id) => self.handle_get_audio_devices(req_id),
            PlayerCommand::SetAudioDevice { id, mode } => {
                self.handle_set_audio_device(id, mode);
            }
            PlayerCommand::LoadFailed {
                error,
                code,
                seq,
                load_gen,
            } => {
                // Superseded load (stop() keeps its task alive; abort is
                // cooperative): checked at processing time, like handle_load's
                // stale-Load gate, so it also covers send/process races.
                if load_gen != LOAD_SEQ.load(Relaxed) {
                    crate::vprintln!("[LOAD #{load_gen}] stale LoadFailed, ignoring");
                    return;
                }
                // Error first (cause), then Duration(0) (effect): the SDK's load()
                // awaits `mediaduration` with no timeout, and 0 is its own
                // unknown-duration sentinel.
                (self.callback)(PlayerEvent::MediaError { error, code });
                (self.callback)(PlayerEvent::Duration(0.0, seq));
            }
            PlayerCommand::EmitMaxConnections => {
                (self.callback)(PlayerEvent::MaxConnectionsReached);
            }
            #[cfg(target_os = "windows")]
            PlayerCommand::SetVolumeSync(enabled) => {
                self.handle_set_volume_sync(enabled);
            }
        }
    }
}
