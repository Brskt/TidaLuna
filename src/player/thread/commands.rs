use super::decode::{DecodeThreadConfig, spawn_decode_thread};
use super::output::{
    format_duration_mmss, format_sample_rate, open_output_stream, probe_audio_format,
};
use super::{DecodeCommand, PlayerThread};
use crate::player::resume::RESUME_MIN_SECONDS;
use crate::player::{
    DeviceErrorKind, LOAD_SEQ, LoadRequest, MediaErrorCode, MediaFormatSnapshot, PlaybackState,
    PlayerCommand, PlayerEvent, ResumePolicy, format_ms, short_id,
};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;

use cpal::traits::StreamTrait;

#[cfg(target_os = "windows")]
use crate::player::asio::host::{AsioCommand, AsioHandle};
#[cfg(target_os = "windows")]
use crate::player::{ASIO_STREAM_SEQ, EXCLUSIVE_STREAM_SEQ, wasapi};
#[cfg(target_os = "windows")]
use std::sync::Arc;
#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicBool, AtomicU64};
#[cfg(target_os = "windows")]
use std::thread;
#[cfg(target_os = "windows")]
use wasapi::ExclusiveCommand;

/// The defined outcomes of a `player.play`, given the player's current state.
/// Pure, to be unit-tested without the audio pipeline.
#[derive(Debug, PartialEq)]
pub(super) enum PlayAction {
    /// A live pipeline exists - resume it.
    Resume,
    /// A load for this generation is genuinely in flight - wait for it.
    DeferTo(u32),
    /// No load is coming, but a previously-loaded source is retained - reload it.
    ReArm,
    /// Nothing is loaded and nothing to re-arm (cold/empty) - do nothing.
    Ignore,
}

/// Decide what a `player.play` does. Deferring is legitimate ONLY while a load
/// is in flight; otherwise a no-track play re-arms the retained source.
pub(super) fn decide_play(
    has_track: bool,
    loading_gen: Option<u32>,
    has_retained_source: bool,
) -> PlayAction {
    match (has_track, loading_gen, has_retained_source) {
        (true, _, _) => PlayAction::Resume,
        (false, Some(generation), _) => PlayAction::DeferTo(generation),
        (false, None, true) => PlayAction::ReArm,
        (false, None, false) => PlayAction::Ignore,
    }
}

/// Apply a `LoadSettled` for `generation`: clear `loading_gen` and any play
/// deferred on it. Gen-matched: a stale settle cannot clear a newer load.
pub(super) fn settle_load(
    loading_gen: Option<u32>,
    pending_play: Option<u32>,
    generation: u32,
) -> (Option<u32>, Option<u32>) {
    (
        loading_gen.filter(|&g| g != generation),
        pending_play.filter(|&g| g != generation),
    )
}

/// Does a seek queued while no bypass decoder was live still apply to the track a load is
/// about to open? Its tag is the only thing that can answer. A restart answers no whatever
/// the tag holds: its contract is position 0.
#[cfg(target_os = "windows")]
pub(super) fn queued_seek_survives(
    queued_track: Option<&str>,
    track_id: &str,
    resume_policy: ResumePolicy,
) -> bool {
    match queued_track {
        Some(tagged) => tagged == track_id && !matches!(resume_policy, ResumePolicy::Restart),
        None => false,
    }
}

/// The queued seek names where the listener asked to be, the auto-resume only where they left
/// off. Every reader of the pair answers here, so none can disagree with the one consuming it.
pub(super) fn resolve_start_position(queued: Option<f64>, auto_resume: Option<f64>) -> Option<f64> {
    queued.or(auto_resume)
}

impl<F: Fn(PlayerEvent) + Send + 'static> PlayerThread<F> {
    pub(super) fn resolve_resume_policy(
        &self,
        resume_policy: ResumePolicy,
        track_id: &str,
    ) -> Option<f64> {
        match resume_policy {
            ResumePolicy::Disabled => {
                if self.allow_startup_auto_resume {
                    self.resume_store.get(track_id)
                } else {
                    None
                }
            }
            // Position 0, whatever the store holds: a fresh play instance of the same
            // track must not inherit where the previous instance stopped.
            ResumePolicy::Restart => None,
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

    /// Queue a seek issued while no bypass decoder is live, tagged with the track it
    /// targets so the upcoming load can tell whether it still applies.
    #[cfg(target_os = "windows")]
    fn queue_user_seek(&mut self, time: f64) {
        match self.current_track_id.clone() {
            Some(track_id) => self.user_seek_override = Some((track_id, time)),
            // A decode failure drops the track identity before the SDK's recovery reload;
            // with nothing to tag, the seek has no load it could survive to.
            None => crate::vprintln!("[SEEK]   queued seek dropped: no track to tag it with"),
        }
    }

    /// No tag re-check here: `handle_load` is the only place that discards one for another.
    #[cfg(target_os = "windows")]
    pub(super) fn take_user_seek_override(&mut self) -> Option<f64> {
        self.user_seek_override.take().map(|(_, time)| time)
    }

    /// The position a bypass stream must open at. Both candidates compete for one slot, and
    /// a loser left armed reaches the next reader of that slot as a fresh seek.
    #[cfg(target_os = "windows")]
    fn take_start_position(&mut self) -> Option<f64> {
        let queued = self.take_user_seek_override();
        let auto_resume = self.pending_resume_seek.take();
        resolve_start_position(queued, auto_resume)
    }

    /// The same answer, consuming neither candidate: a load announces where it will open
    /// before the backend that consumes the pair has been chosen.
    fn start_position(&self) -> Option<f64> {
        #[cfg(target_os = "windows")]
        let queued = self.user_seek_override.as_ref().map(|(_, time)| *time);
        #[cfg(not(target_os = "windows"))]
        let queued: Option<f64> = None;
        resolve_start_position(queued, self.pending_resume_seek)
    }

    pub(super) fn stop_decode(&mut self) {
        if let Some(tx) = self.decode_cmd_tx.take() {
            let _ = tx.send(DecodeCommand::Stop);
        }
        self.cpal_stream = None;
        if let Some(handle) = self.decode_handle.take() {
            let _ = handle.join();
        }
        self.decode_event_rx = None;
        // Nothing survives to answer a seek sent to the thread just retired, and a pin left
        // here follows the next track: pinned progress bar, suppressed resume writes.
        self.seeking = false;
        self.seek_target = None;
        self.seek_wall_start = None;
    }

    /// Mint the identity of a new seek. Every dispatch goes through here, live or folded
    /// into a decoder spawn, which is why there is no per-stream reset to forget: the
    /// counter only ever moves forward, putting every earlier ack permanently out of reach.
    pub(super) fn next_seek_gen(&mut self) -> u32 {
        self.seek_ack_gen = self.seek_ack_gen.wrapping_add(1);
        self.seek_ack_gen
    }

    /// Spawn the exclusive decoder for `buffer`, seeking the source to `seek_to`.
    /// A fresh `stream_id` makes the render drop the prior decoder's stale
    /// PushPcm. No `Stop` (it emits `Stopped`, which clears the host-side track);
    /// the new `StartStream` is the reset.
    #[cfg(target_os = "windows")]
    pub(super) fn spawn_exclusive_decoder(
        &mut self,
        buffer: crate::player::buffer::RamBuffer,
        seek_to: Option<f64>,
        start_paused: bool,
    ) {
        let Some(cmd_tx) = self.exclusive_handle.as_ref().map(|h| h.command_sender()) else {
            return;
        };
        if let Some(prev) = self.exclusive_stream_cancel.take() {
            prev.store(true, Relaxed);
            // Wake the retired reader to see the cancel and quiesce instead
            // of parking up to the read timeout while the new stream contends for
            // the same buffer (matches handle_set_audio_device).
            if let Some(ref buf) = self.current_buffer {
                buf.wake_readers();
            }
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.exclusive_stream_cancel = Some(cancel.clone());

        let (seek_tx, seek_rx) = mpsc::channel::<(f64, u32)>();
        self.exclusive_seek_tx = Some(seek_tx);
        // Minted here, beside the stream_id, rather than at any call site: one caller
        // already forgot the sibling `seeking` reset, and the callee cannot be skipped.
        let seek_gen_id = self.next_seek_gen();
        // Per-stream consumed counter for the decoder's sent-minus-consumed throttle.
        let consumed = Arc::new(AtomicU64::new(0));
        // Fresh stream: don't inherit a stale reverse-to-shared position from a prior
        // exclusive session (the buffer-reuse mode switch bypasses handle_load's clear).
        self.last_exclusive_pos = None;

        let reader = buffer.clone().with_reader_cancel(cancel.clone());
        let total_len = buffer.total_len();
        let stream_id = EXCLUSIVE_STREAM_SEQ.fetch_add(1, Relaxed) + 1;
        // Record the live stream_id to keep Play/Pause stream-scoped: a premature
        // or stale command must not act on a superseded render context (mirrors
        // current_asio_stream_id).
        self.current_exclusive_stream_id = Some(stream_id);
        thread::spawn(move || {
            if let Err(e) = wasapi::stream_flac_reader_to_wasapi(
                reader,
                total_len,
                stream_id,
                cmd_tx,
                cancel.clone(),
                seek_to,
                seek_gen_id,
                start_paused,
                seek_rx,
                consumed,
            ) && !cancel.load(Relaxed)
            {
                crate::vprintln!("[WASAPI] Stream decode failed: {e}");
            }
        });

        self.current_buffer = Some(buffer);
        self.has_track = true;
        self.is_playing = !start_paused;
        // Playback starting ends the queued-seek window; `take_start_position` callers have
        // emptied it already. This retires the device-switch spawn path, where a seek left
        // armed would surface at the next pause and drag playback back to a stale position.
        self.user_seek_override = None;
    }

    #[cfg(target_os = "windows")]
    pub(super) fn start_exclusive_playback(
        &mut self,
        buffer: crate::player::buffer::RamBuffer,
        start_paused: bool,
    ) -> bool {
        if !self.is_exclusive_mode {
            return false;
        }
        // The new StartStream re-bases the render in place (no teardown that would
        // null the just-loaded track on a playlist-advance). A queued user seek wins
        // over the load-time resume position.
        let seek_to = self.take_start_position();
        self.spawn_exclusive_decoder(buffer, seek_to, start_paused);
        true
    }

    /// Spawn the ASIO decoder for `buffer`, seeking the source to `seek_to`. Mirrors
    /// `spawn_exclusive_decoder`: a fresh `stream_id` makes the control thread drop
    /// the prior decoder's stale `PushPcm`; the new `StartStream` is the reset.
    #[cfg(target_os = "windows")]
    pub(super) fn spawn_asio_decoder(
        &mut self,
        buffer: crate::player::buffer::RamBuffer,
        seek_to: Option<f64>,
        start_paused: bool,
    ) {
        let Some(cmd_tx) = self.asio_handle.as_ref().map(|h| h.command_sender()) else {
            return;
        };
        if let Some(prev) = self.asio_stream_cancel.take() {
            prev.store(true, Relaxed);
            // Wake the retired reader to quiesce instead of parking on the read
            // timeout while the new stream contends for the same buffer.
            if let Some(ref buf) = self.current_buffer {
                buf.wake_readers();
            }
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.asio_stream_cancel = Some(cancel.clone());

        let (seek_tx, seek_rx) = mpsc::channel::<(f64, u32)>();
        self.asio_seek_tx = Some(seek_tx);
        // Minted here, beside the stream_id, rather than at any call site: one caller
        // already forgot the sibling `seeking` reset, and the callee cannot be skipped.
        let seek_gen_id = self.next_seek_gen();
        // Per-stream consumed counter for the decoder's sent-minus-consumed throttle.
        let consumed = Arc::new(AtomicU64::new(0));

        let reader = buffer.clone().with_reader_cancel(cancel.clone());
        let stream_id = ASIO_STREAM_SEQ.fetch_add(1, Relaxed) + 1;
        // Record the live stream_id, letting poll_asio_events reject stale Stopped/Completed
        // events from a superseded stream (otherwise they null a newer track -> double-load).
        self.current_asio_stream_id = Some(stream_id);
        // A fresh stream has no pending live seek; clear any stale seek-pin from a previous
        // track: its target must not pin this track's progress bar.
        self.seeking = false;
        self.seek_target = None;
        // The buffer-reuse mode switch spawns this directly, bypassing handle_load (which
        // otherwise clears these): a fresh stream must not inherit a stale progress-watchdog
        // timer (it would trip "no progress for 2s" instantly and force a shared fallback)
        // or a stale reverse-to-shared position from a prior ASIO session.
        self.asio_watchdog_at = None;
        self.last_asio_pos = None;
        thread::spawn(move || {
            if let Err(e) = crate::player::asio::host::stream_reader_to_asio(
                reader,
                stream_id,
                cmd_tx,
                cancel.clone(),
                seek_to,
                seek_gen_id,
                start_paused,
                seek_rx,
                consumed,
            ) && !cancel.load(Relaxed)
            {
                crate::vprintln!("[ASIO] Stream decode failed: {e}");
            }
        });

        self.current_buffer = Some(buffer);
        self.has_track = true;
        self.is_playing = !start_paused;
        // Playback starting ends the queued-seek window; `take_start_position` callers have
        // emptied it already. This retires the device-switch spawn path, where a seek left
        // armed would surface at the next pause and drag playback back to a stale position.
        self.user_seek_override = None;
    }

    #[cfg(target_os = "windows")]
    pub(super) fn start_asio_playback(
        &mut self,
        buffer: crate::player::buffer::RamBuffer,
        start_paused: bool,
    ) -> bool {
        if !self.is_asio_mode {
            return false;
        }
        // Self-heal after a terminal-stop release: still in ASIO mode but the
        // handle was shut down; respawn it for this load to rebuild the pipeline
        // instead of spawn_asio_decoder silently no-oping on a missing handle.
        // A parked teardown must drain first (one driver instance at a time).
        if self.asio_handle.is_none() {
            // A parked switch owns the next handle: `current_device_id` still names the
            // device the user left, and the replay undoes this respawn, position included.
            if self.pending_device_switch.is_some() {
                self.is_asio_mode = false;
                crate::vprintln!("[ASIO] a device switch is parked; this load plays shared");
                return false;
            }
            if !self.reap_asio_teardown_within(std::time::Duration::from_secs(2)) {
                // Leave ASIO mode: the shared pipeline this load falls back to is then
                // actually played and polled; the next devices.set re-assert
                // re-engages ASIO (parked behind the teardown if still draining).
                self.is_asio_mode = false;
                crate::vprintln!("[ASIO] teardown still draining; this load plays shared");
                return false;
            }
            self.asio_handle = Some(AsioHandle::spawn(
                self.exclusive_gain.clone(),
                self.current_device_id.clone(),
            ));
        }
        // A queued user seek wins over the load-time resume position.
        let seek_to = self.take_start_position();
        self.spawn_asio_decoder(buffer, seek_to, start_paused);
        true
    }

    /// Resolve the configured output device (falling back to the OS default) and
    /// record its concrete name in `current_output_name`, which the shared
    /// re-assert guard compares against. Every cpal-open path resolves through here.
    pub(super) fn resolve_output_device(&mut self) -> Option<cpal::Device> {
        match super::output::resolve_device(self.current_device_id.as_deref()) {
            Some(d) => {
                let name = super::output::output_device_name(&d);
                self.output_is_default = self
                    .current_device_id
                    .as_deref()
                    .is_none_or(super::output::is_default_selector);
                // A named (non-default) request that didn't resolve to itself fell
                // back to the OS default.
                if !self.output_is_default
                    && let Some(req) = self.current_device_id.as_deref()
                    && name.as_deref() != Some(req)
                {
                    crate::vprintln!(
                        "[AUDIO] Device '{}' not found, falling back to default",
                        req
                    );
                }
                self.current_output_name = name;
                Some(d)
            }
            None => {
                (self.callback)(PlayerEvent::DeviceError(DeviceErrorKind::NotFound));
                None
            }
        }
    }

    pub(super) fn handle_load(
        &mut self,
        req: LoadRequest,
        #[allow(unused_variables)] auto_play: bool,
    ) -> bool {
        let LoadRequest {
            buffer,
            load_gen,
            seq: event_seq,
            track_id,
            resume_policy,
            load_start,
            cached,
            format,
        } = req;
        if load_gen != LOAD_SEQ.load(Relaxed) {
            crate::vprintln!("[LOAD #{load_gen}] stale Load, ignoring");
            return false;
        }

        if let Some(ref prev) = self.current_track_id
            && *prev != track_id
        {
            self.resume_store.clear(prev);
        }

        self.current_track_id = Some(track_id.clone());
        self.set_committed_track(Some((track_id.clone(), format.clone())));
        self.pending_resume_seek = self.resolve_resume_policy(resume_policy, &track_id);
        self.current_seq = event_seq;
        self.is_cached = cached;
        self.current_format = format;
        // Invalidated until this load's probe emits a fresh one (the ASIO/exclusive
        // branches never do -> stays None, and a re-assert cannot send a stale format).
        self.last_media_format = None;
        self.buffer_stalled = false;
        self.pending_complete = false;
        self.last_played_snapshot = 0;
        // A fresh load announces Ready; a seek settled on it must put Ready back, not the
        // Stopped or Paused the previous track left behind.
        self.idle_state = PlaybackState::Ready;

        crate::vprintln!(
            "[LOAD #{load_gen}] handle_load enter | cached={} | track={}",
            cached,
            short_id(&track_id, 60)
        );
        let handle_start = std::time::Instant::now();

        // Cancel previous playback
        #[cfg(target_os = "windows")]
        {
            if let Some(cancel) = self.exclusive_stream_cancel.take() {
                cancel.store(true, Relaxed);
            }
            self.exclusive_seek_tx = None;
            if let Some(cancel) = self.asio_stream_cancel.take() {
                cancel.store(true, Relaxed);
            }
            self.asio_seek_tx = None;
            // A new track invalidates any retained exclusive/asio position.
            self.last_exclusive_pos = None;
            self.last_asio_pos = None;
            // Re-anchor the ASIO progress watchdog: a stale timer from a prior stream (e.g. after
            // a shared fallback) must not trip 2s into the fresh track before its clock re-arms it.
            self.asio_watchdog_at = None;
            // The per-track ASIO/exclusive skips clear when a DIFFERENT track loads (a same-track
            // re-arm keeps them: the unsupported track stays shared instead of re-engaging).
            if self
                .asio_skip_track
                .as_deref()
                .is_some_and(|s| s != track_id.as_str())
            {
                self.asio_skip_track = None;
            }
            if self
                .exclusive_skip_track
                .as_deref()
                .is_some_and(|s| s != track_id.as_str())
            {
                self.exclusive_skip_track = None;
            }
            // A queued user seek carries the track it was issued against, which is what lets
            // it survive the re-arm of that same track: outliving the load it waits for is
            // its whole purpose.
            if !queued_seek_survives(
                self.user_seek_override.as_ref().map(|(id, _)| id.as_str()),
                &track_id,
                resume_policy,
            ) {
                self.user_seek_override = None;
            }
            // A new load means the prior stop was a track change, not a real stop:
            // cancel any pending device release; the device is never freed mid-change.
            self.exclusive_release_at = None;
            self.asio_release_at = None;
        }
        if let Some(ref old_buf) = self.current_buffer {
            old_buf.cancel();
        }
        self.stop_decode();

        // Eagerly report the load's resolved start position: the bar snaps to it immediately
        // rather than lingering on the previous track until the backend's first periodic
        // report. Safe: same-track re-asserts never reach here, and mediacurrenttime is
        // store-only, which keeps this from feeding back as a seek.
        (self.callback)(PlayerEvent::TimeUpdate(
            self.start_position().unwrap_or(0.0),
            self.current_seq,
        ));

        let teardown_ms = handle_start.elapsed().as_secs_f64() * 1000.0;
        let decode_start = std::time::Instant::now();

        // A play that raced ahead of this load (tagged by load_gen) is FOLDED into the
        // bypass stream's own start (start_paused=false) instead of a separate Play: the
        // decoder sends StartStream only after probing; a separate Play would race the
        // adoption. Same intent-travels-with-the-command shape as want_play.
        #[cfg(target_os = "windows")]
        let deferred_play = self.pending_play == Some(load_gen);

        // ASIO path (mutually exclusive with WASAPI-exclusive). Same start_paused
        // contract; start_asio_playback consumes the resume position. Skipped for a track the
        // device can't clock in ASIO (RateUnsupported), leaving it shared without re-engaging.
        #[cfg(target_os = "windows")]
        if self.asio_skip_track.as_deref() != Some(track_id.as_str())
            && self.start_asio_playback(buffer.clone(), !(auto_play || deferred_play))
        {
            if deferred_play {
                self.pending_play = None;
                crate::state::GOVERNOR
                    .buffer_progress()
                    .set_playback_active(true);
                crate::vprintln!(
                    "[PLAY]   deferred play folded into ASIO start (load #{load_gen})"
                );
            }
            crate::vprintln!(
                "[ASIO] Progressive decode started ({:.0}ms setup)",
                decode_start.elapsed().as_secs_f64() * 1000.0
            );
            return true;
        }

        // Exclusive path. start_paused mirrors ASIO (a paused restore enters paused,
        // a deferred play folds into the start); start_exclusive_playback consumes the
        // resume position. Skipped for a track whose format the device can't do in
        // exclusive (FormatUnsupported), leaving it shared without re-engaging exclusive.
        #[cfg(target_os = "windows")]
        if self.exclusive_skip_track.as_deref() != Some(track_id.as_str())
            && self.start_exclusive_playback(buffer.clone(), !(auto_play || deferred_play))
        {
            if deferred_play {
                self.pending_play = None;
                crate::state::GOVERNOR
                    .buffer_progress()
                    .set_playback_active(true);
                crate::vprintln!(
                    "[PLAY]   deferred play folded into exclusive start (load #{load_gen})"
                );
            }
            crate::vprintln!(
                "[WASAPI] Progressive decode started ({:.0}ms setup)",
                decode_start.elapsed().as_secs_f64() * 1000.0
            );
            return true;
        }

        // Shared mode: symphonia + cpal
        let total_len = buffer.total_len();

        let probe = match probe_audio_format(&buffer) {
            Ok(p) => p,
            Err(e) => {
                crate::vprintln!("[ERROR]  {e}");
                (self.callback)(PlayerEvent::MediaError {
                    error: e,
                    code: MediaErrorCode::UnreadableFile,
                });
                // Settle the SDK's load() (its `mediaduration` await has no
                // timeout); 0, not the previous track's stale current_duration.
                (self.callback)(PlayerEvent::Duration(0.0, self.current_seq));
                // This load failed; drop the reconcile signal committed at entry
                // or a same-track reload would resume a pipeline that never built.
                self.set_committed_track(None);
                return false;
            }
        };
        let probe_ms = decode_start.elapsed().as_secs_f64() * 1000.0;
        crate::vprintln!("[LOAD #{load_gen}] probe: {}", format_ms(probe_ms));

        let source_sample_rate = probe.sample_rate;
        let source_channels = probe.channels;
        let source_duration = probe.duration;
        let source_bit_depth = probe.bit_depth;
        let source_codec = probe.codec;

        self.current_duration = source_duration;
        self.decoded_samples.store(0, Relaxed);
        self.played_samples.store(0, Relaxed);

        // Emit version once (fire-once at first load)
        if !self.version_emitted {
            self.version_emitted = true;
            (self.callback)(PlayerEvent::Version(env!("CARGO_PKG_VERSION")));
        }

        let media_format = MediaFormatSnapshot {
            codec: source_codec,
            sample_rate: source_sample_rate,
            bit_depth: source_bit_depth,
            channels: source_channels,
            bytes: total_len,
        };
        self.last_media_format = Some(media_format);
        (self.callback)(media_format.to_event());

        // Open cpal stream
        let device = match self.resolve_output_device() {
            Some(d) => d,
            None => {
                self.set_committed_track(None);
                return false;
            }
        };

        let cpal_start = std::time::Instant::now();
        let opened =
            match open_output_stream(&device, source_sample_rate, source_channels, &self.volume) {
                Some(o) => o,
                None => {
                    (self.callback)(PlayerEvent::DeviceError(
                        DeviceErrorKind::FormatNotSupported,
                    ));
                    self.set_committed_track(None);
                    return false;
                }
            };
        let cpal_ms = cpal_start.elapsed().as_secs_f64() * 1000.0;
        crate::vprintln!("[LOAD #{load_gen}] cpal open: {}", format_ms(cpal_ms));

        let actual_rate = opened.rate;
        let actual_channels = opened.channels;
        let stream = opened.stream;
        let ring_producer = opened.producer;
        let seek_gen = opened.seek_gen;
        self.cpal_muted = Some(opened.muted);
        self.cpal_mute_ack = Some(opened.mute_ack);
        self.cpal_stream_error = Some(opened.stream_error);
        self.played_samples = opened.played_samples;

        self.sample_rate = actual_rate;
        self.channels = actual_channels;

        let (decode_cmd_tx, decode_cmd_rx) = mpsc::channel();
        let (decode_event_tx, decode_event_rx) = mpsc::channel();
        let decoded_samples = self.decoded_samples.clone();

        let decode_buffer = buffer.clone();
        let decode_handle = spawn_decode_thread(DecodeThreadConfig {
            buffer: decode_buffer,
            producer: ring_producer,
            decoded_samples,
            cmd_rx: decode_cmd_rx,
            event_tx: decode_event_tx,
            output_rate: actual_rate,
            output_channels: actual_channels,
            seek_gen,
        });

        self.cpal_stream = Some(stream);
        self.decode_cmd_tx = Some(decode_cmd_tx);
        self.decode_event_rx = Some(decode_event_rx);
        self.decode_handle = Some(decode_handle);
        self.current_buffer = Some(buffer);
        self.has_track = true;
        self.is_playing = false;
        // Where a parked switch or a draining teardown lands. Folding the queued seek into the
        // slot the pre-seek reads applies it here instead of swallowing it; taking it stops the
        // drift into whichever bypass mode returns.
        #[cfg(target_os = "windows")]
        if let Some(queued) = self.take_user_seek_override() {
            self.pending_resume_seek = Some(queued);
        }

        // Volume sync: only init once - rebinding at each track causes drift because
        // the PID-based session lookup can pick a stale/wrong session during transitions.
        // Re-init happens on device switch (device.rs) or toggle (handle_set_volume_sync).
        #[cfg(target_os = "windows")]
        if self.volume_sync.is_none() {
            self.init_volume_sync();
        }

        // Pre-seek
        self.pre_seek_pos = None;
        if let Some(pos) = self.pending_resume_seek {
            // Minted before the borrow below: the ack carries it back, and a pre-seek that
            // went unidentified would be settled by whatever gen an unrelated seek left.
            let gen_id = self.next_seek_gen();
            if let Some(ref tx) = self.decode_cmd_tx {
                let _ = tx.send(DecodeCommand::Seek(pos, gen_id));
                self.pre_seek_pos = Some(pos);
                crate::vprintln!("[LOAD #{load_gen}] pre-seek to {:.1}s (decode paused)", pos);
            }
        }

        if self.current_duration > 0.0 {
            (self.callback)(PlayerEvent::Duration(
                self.current_duration,
                self.current_seq,
            ));
        }

        let bitrate = if self.current_duration > 0.0 {
            (total_len as f64 * 8.0 / self.current_duration / 1000.0) as u32
        } else {
            0
        };
        let bitrate_bps = if self.current_duration > 0.0 {
            (total_len as f64 / self.current_duration) as u64
        } else {
            0
        };
        {
            let bp = crate::state::GOVERNOR.buffer_progress();
            bp.bitrate_bps.store(bitrate_bps, Relaxed);
            bp.total_len.store(total_len, Relaxed);
            if let Some(ref buf) = self.current_buffer {
                bp.written.store(buf.written(), Relaxed);
                bp.read_pos.store(buf.read_cursor(), Relaxed);
            }
        }

        crate::vprintln!(
            "[CODEC]  {} / {}ch | {} kbps | {}",
            format_sample_rate(source_sample_rate),
            source_channels,
            bitrate,
            format_duration_mmss(self.current_duration)
        );
        crate::vprintln!(
            "[LOAD #{load_gen}] pipeline: teardown={} probe={} cpal={} total={}{}",
            format_ms(teardown_ms),
            format_ms(probe_ms),
            format_ms(cpal_ms),
            format_ms(handle_start.elapsed().as_secs_f64() * 1000.0),
            if cached {
                " (CACHE HIT)"
            } else {
                " (streaming)"
            }
        );
        crate::vprintln!(
            "[LOAD #{load_gen}] ready in {} (from load_with_policy entry)",
            format_ms(load_start.elapsed().as_secs_f64() * 1000.0)
        );

        (self.callback)(PlayerEvent::StateChange(
            PlaybackState::Ready,
            self.current_seq,
        ));

        // Honor a play that raced ahead of this load. Tagged by load_gen: a
        // play meant for a track the user has since skipped past is not applied
        // here (that intent's generation won't match this one).
        if self.pending_play == Some(load_gen) {
            self.pending_play = None;
            crate::vprintln!("[PLAY]   applying deferred play for load #{load_gen}");
            self.handle_play();
        }
        true
    }

    pub(super) fn handle_load_started(&mut self, generation: u32) {
        // Accept only the current generation: a stale LoadStarted (a tokio load
        // racing an IPC load) must not regress loading_gen past a newer load/stop.
        if generation == LOAD_SEQ.load(Relaxed) {
            self.loading_gen = Some(generation);
        }
    }

    pub(super) fn handle_load_settled(&mut self, generation: u32) {
        let (loading_gen, pending_play) =
            settle_load(self.loading_gen, self.pending_play, generation);
        self.loading_gen = loading_gen;
        self.pending_play = pending_play;
    }

    pub(super) fn handle_play(&mut self) {
        self.allow_startup_auto_resume = false;
        // Resuming cancels a pending real-stop device release (the user came back).
        #[cfg(target_os = "windows")]
        {
            self.exclusive_release_at = None;
            self.asio_release_at = None;
        }

        // Capture the retained source only when no track and no load in flight;
        // the short-circuit skips the CURRENT_TRACK lock on the resume path.
        let retained = if !self.has_track && self.loading_gen.is_none() {
            crate::state::CURRENT_TRACK
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        } else {
            None
        };
        match decide_play(self.has_track, self.loading_gen, retained.is_some()) {
            PlayAction::DeferTo(generation) => {
                // A load is genuinely in flight: handle_load applies this play
                // when it delivers for the matching generation.
                self.pending_play = Some(generation);
                crate::vprintln!(
                    "[PLAY]   deferred until load #{generation} is ready (load in flight)"
                );
                return;
            }
            PlayAction::ReArm => {
                // No load coming but a source is retained: hand the captured
                // track to flush.rs (avoids a second CURRENT_TRACK lock).
                crate::vprintln!("[PLAY]   no live pipeline; re-arming retained source");
                if let Some(track) = retained {
                    // Resume at the retained source's last position, not 0 (e.g. a >10s
                    // pause released the device, re-arming here). resume_store holds it
                    // but is cleared on track-end; a post-Completed re-arm gets None
                    // and starts at 0; the mode's own live position is the floor-free
                    // fast-path.
                    let replayed = crate::player::canonical_track_id(&track.url);
                    let position = self.resume_store.get(&replayed);
                    #[cfg(target_os = "windows")]
                    let position = if self.is_asio_mode {
                        self.last_asio_pos.or(position)
                    } else {
                        self.last_exclusive_pos.or(position)
                    };
                    // A seek taken while the device was released outranks both the live
                    // mirror and the store: it names where the user asked to be. Read rather
                    // than taken, since the load this replay triggers is what consumes it.
                    #[cfg(target_os = "windows")]
                    let position = self
                        .user_seek_override
                        .as_ref()
                        .filter(|(id, _)| *id == replayed)
                        .map(|(_, time)| *time)
                        .or(position);
                    (self.callback)(PlayerEvent::ReplayRequest {
                        track,
                        expected_gen: LOAD_SEQ.load(Relaxed),
                        position,
                        play: true,
                    });
                }
                return;
            }
            PlayAction::Ignore => {
                crate::vprintln!("[PLAY]   play with no track and no retained source; ignoring");
                return;
            }
            // Fall through to the live-pipeline resume below.
            PlayAction::Resume => {}
        }
        self.pending_play = None;

        #[cfg(target_os = "windows")]
        {
            if self.is_asio_mode {
                if let Some(seek_time) = self.take_start_position() {
                    // Announce the position something actually took, as handle_seek does:
                    // a queued seek can outlive its decoder, and a park leaves none at all.
                    let target = seek_time.max(0.0);
                    let gen_id = self.next_seek_gen();
                    let dispatched = self
                        .asio_seek_tx
                        .as_ref()
                        .is_some_and(|tx| tx.send((seek_time, gen_id)).is_ok());
                    if dispatched {
                        self.last_asio_pos = Some(target);
                        (self.callback)(PlayerEvent::TimeUpdate(target, self.current_seq));
                    } else if self.asio_handle.is_some()
                        && let Some(buffer) = self.current_buffer.clone()
                    {
                        // A respawn carries the position, making the announced target reachable.
                        self.last_asio_pos = Some(target);
                        self.spawn_asio_decoder(buffer, Some(seek_time), false);
                        (self.callback)(PlayerEvent::TimeUpdate(target, self.current_seq));
                    } else {
                        // Nothing took it, and spawn_asio_decoder no-ops without a handle.
                        let live = self.last_asio_pos.unwrap_or(0.0);
                        (self.callback)(PlayerEvent::TimeUpdate(live, self.current_seq));
                        crate::vprintln!(
                            "[PLAY]   asio: no decoder took the queued seek; position restored"
                        );
                    }
                } else if let Some(ref handle) = self.asio_handle {
                    // Only resume the live retained stream. A missing id means there
                    // is no retained stream to resume; sending id 0 would be silently
                    // dropped by the stream-scoped guard; log it instead.
                    match self.current_asio_stream_id {
                        Some(stream_id) => {
                            handle.send(AsioCommand::Play { stream_id });
                        }
                        None => {
                            crate::vprintln!("[ASIO]   play: no live stream id; nothing to resume");
                        }
                    }
                }
                self.is_playing = true;
                crate::state::GOVERNOR
                    .buffer_progress()
                    .set_playback_active(true);
                return;
            }
        }

        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                if let Some(seek_time) = self.take_start_position() {
                    // Announce the position something actually took, as handle_seek does:
                    // a decoder that died without reporting leaves a sender that accepts
                    // nothing, and presence alone reads as success.
                    let target = seek_time.max(0.0);
                    let gen_id = self.next_seek_gen();
                    let dispatched = self
                        .exclusive_seek_tx
                        .as_ref()
                        .is_some_and(|tx| tx.send((seek_time, gen_id)).is_ok());
                    if dispatched {
                        self.last_exclusive_pos = Some(target);
                        (self.callback)(PlayerEvent::TimeUpdate(target, self.current_seq));
                    } else if self.exclusive_handle.is_some()
                        && let Some(buffer) = self.current_buffer.clone()
                    {
                        // A respawn carries the position, making the announced target reachable.
                        self.last_exclusive_pos = Some(target);
                        self.spawn_exclusive_decoder(buffer, Some(seek_time), false);
                        (self.callback)(PlayerEvent::TimeUpdate(target, self.current_seq));
                    } else {
                        // Nothing took it, and spawn_exclusive_decoder no-ops without a handle.
                        let live = self.last_exclusive_pos.unwrap_or(0.0);
                        (self.callback)(PlayerEvent::TimeUpdate(live, self.current_seq));
                        crate::vprintln!(
                            "[PLAY]   exclusive: no decoder took the queued seek; position restored"
                        );
                    }
                } else if let Some(ref handle) = self.exclusive_handle {
                    // Only resume the live adopted stream (mirrors the ASIO branch):
                    // a missing id means there is no stream to resume yet.
                    match self.current_exclusive_stream_id {
                        Some(stream_id) => {
                            handle.send(ExclusiveCommand::Play { stream_id });
                        }
                        None => {
                            crate::vprintln!("[WASAPI] play: no live stream id; nothing to resume");
                        }
                    }
                }
                self.is_playing = true;
                crate::state::GOVERNOR
                    .buffer_progress()
                    .set_playback_active(true);
                return;
            }
        }

        self.is_playing = true;
        crate::state::GOVERNOR
            .buffer_progress()
            .set_playback_active(true);
        (self.callback)(PlayerEvent::StateChange(
            PlaybackState::Active,
            self.current_seq,
        ));

        // The intent is spent either way. Announce where the reader landed, never the request:
        // a refused pre-seek left the decoder where it was.
        self.pending_resume_seek = None;
        if let Some(landed) = self.pre_seek_pos {
            (self.callback)(PlayerEvent::TimeUpdate(landed.max(0.0), self.current_seq));
            crate::vprintln!("[PLAY]   start at pre-seeked {:.1}s", landed);
        } else {
            // Nothing landed says nothing about where the decoder sits: a resume after a
            // paused seek starts at that target, not at zero.
            crate::vprintln!("[PLAY]   start at {:.1}s", self.effective_position());
        }
        // A prior device-loss recovery may have torn down the stream (e.g. the device
        // was held exclusively by a fullscreen game). Resuming is a natural retry point:
        // rebuild on the current default device, which is usually free again by now.
        if self.cpal_stream.is_none() {
            self.recover_audio_device();
        } else {
            self.start_playback();
        }
    }

    pub(super) fn start_playback(&mut self) {
        // Both fields are set and cleared together everywhere, and both callers reach here only with
        // a stream: the old `else` arms were dead; a missing stream already surfaces as a
        // `DeviceError` from `recover_audio_device`, which runs instead of this. `stream.play()`
        // failing is the one real failure left and nothing else reports it. It is logged ungated.
        if let Some(ref stream) = self.cpal_stream {
            match stream.play() {
                Ok(()) => crate::vprintln!("[PLAY]   cpal stream.play() OK"),
                Err(e) => crate::verr!("[ERROR]  cpal stream.play() failed: {e}"),
            }
        }
        if let Some(ref tx) = self.decode_cmd_tx {
            let _ = tx.send(DecodeCommand::Resume);
            crate::vprintln!("[PLAY]   DecodeCommand::Resume sent");
        }
        self.pre_seek_pos = None;
    }

    pub(super) fn try_skip_pre_seek(&mut self, target: f64) -> bool {
        if let Some(pre_pos) = self.pre_seek_pos.take()
            && (pre_pos - target).abs() < super::PRE_SEEK_TOLERANCE
        {
            (self.callback)(PlayerEvent::TimeUpdate(target.max(0.0), self.current_seq));
            return true;
        }
        false
    }

    pub(super) fn handle_pause(&mut self) {
        // A pause cancels any play deferred while the track was still loading;
        // otherwise handle_load would auto-play it against this pause intent.
        self.pending_play = None;
        // User pause: a same-track re-assert must not auto-resume. Set above the
        // windows early-returns, covering every mode (see resume_on_reassert).
        self.resume_on_reassert = false;
        // A pause is not a stop; handle_stop overwrites this after calling here. Ahead of
        // the windows early-returns like resume_on_reassert, letting every mode record it.
        self.idle_state = PlaybackState::Paused;
        #[cfg(target_os = "windows")]
        {
            if self.is_asio_mode {
                let was_playing = self.is_playing;
                if let Some(ref handle) = self.asio_handle {
                    match self.current_asio_stream_id {
                        Some(stream_id) => {
                            handle.send(AsioCommand::Pause { stream_id });
                        }
                        None => {
                            crate::vprintln!("[ASIO]   pause: no live stream id; nothing to pause");
                        }
                    }
                }
                self.is_playing = false;
                // ASIO pause keeps the driver clock, but its exclusive claim blocks
                // other apps: release it on a sustained pause. A short pause/resume
                // or a stop->load cancels this in time (handle_play/handle_load);
                // resume after a release respawns the driver (self-heal).
                if was_playing && self.asio_handle.is_some() {
                    self.asio_release_at =
                        Some(std::time::Instant::now() + super::ASIO_IDLE_RELEASE);
                }
                crate::state::GOVERNOR
                    .buffer_progress()
                    .set_playback_active(false);
                self.resume_store.flush_if_due(true);
                return;
            }
        }

        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                let was_playing = self.is_playing;
                if let Some(ref handle) = self.exclusive_handle {
                    match self.current_exclusive_stream_id {
                        Some(stream_id) => {
                            handle.send(ExclusiveCommand::Pause { stream_id });
                        }
                        None => {
                            crate::vprintln!("[WASAPI] pause: no live stream id; nothing to pause");
                        }
                    }
                }
                self.is_playing = false;
                // Hold the device, then release it on a sustained pause, letting other
                // apps regain the DAC (TIDAL has no stop button -> pause is the stop signal).
                // A short pause/resume or a track-change stop->load cancels this in time
                // (handle_play/handle_load): only a real lingering pause fires.
                if was_playing {
                    self.exclusive_release_at =
                        Some(std::time::Instant::now() + super::EXCLUSIVE_PAUSE_RELEASE);
                }
                crate::state::GOVERNOR
                    .buffer_progress()
                    .set_playback_active(false);
                self.resume_store.flush_if_due(true);
                return;
            }
        }

        if let Some(ref stream) = self.cpal_stream {
            let _ = stream.pause();
        }
        if let Some(ref tx) = self.decode_cmd_tx {
            let _ = tx.send(DecodeCommand::Pause);
        }

        let pos_secs = self.effective_position();
        (self.callback)(PlayerEvent::TimeUpdate(pos_secs, self.current_seq));

        self.is_playing = false;
        crate::state::GOVERNOR
            .buffer_progress()
            .set_playback_active(false);
        (self.callback)(PlayerEvent::StateChange(
            PlaybackState::Paused,
            self.current_seq,
        ));
        self.resume_store.flush_if_due(true);
    }

    pub(super) fn handle_stop(&mut self, event_seq: u32) {
        // Reconciliation: an SDK stop is treated as pause-retain, not teardown.
        // The pipeline (buffer, has_track, current_track_id, CURRENT_TRACK, and
        // committed_track) is kept: a same-track re-assert (a quality-swap's
        // cancel-redispatch) resumes in place instead of rebuilding. A genuine
        // track change tears the previous track down when load() switches to a
        // different track (load_with_policy aborts the task + cancels the
        // download, and handle_load replaces the buffer). Position is preserved.
        self.current_seq = event_seq;
        // Capture is_playing before handle_pause clears it, then record it as the
        // re-assert resume intent (a stop is a re-assert candidate).
        let was_playing = self.is_playing;
        self.handle_pause();
        self.resume_on_reassert = was_playing;
        // handle_pause above arms the release only when something was playing; a
        // terminal stop (the SDK's reset(); nothing has to follow) must release
        // even from an already-paused session: re-arm without a was_playing gate.
        #[cfg(target_os = "windows")]
        if self.is_asio_mode && self.asio_handle.is_some() {
            self.asio_release_at = Some(std::time::Instant::now() + super::ASIO_IDLE_RELEASE);
        }
        // Surface the SDK/UI/Connect/SMTC-visible "stopped" state even though the
        // pipeline is retained internally (handle_pause emits Paused first; the
        // final state observers settle on is Stopped).
        self.idle_state = PlaybackState::Stopped;
        (self.callback)(PlayerEvent::StateChange(
            PlaybackState::Stopped,
            self.current_seq,
        ));
    }

    /// A seek persists its target only once a backend has accepted it. Persisting on entry
    /// left a refused seek in the store, and a later auto-resume honoured a position
    /// playback never reached. The deferred branches persist nothing: the load they wait on
    /// may never land.
    pub(super) fn handle_seek(&mut self, time: f64) {
        // Latest-seek-wins
        let mut latest_time = time;
        while let Ok(next_cmd) = self.cmd_rx.try_recv() {
            match next_cmd {
                PlayerCommand::Seek(t) => {
                    latest_time = t;
                }
                other => self.pending_cmds.push(other),
            }
        }

        // Every backend turns this into a `symphonia` Time, which rejects anything past i64
        // seconds and returns nothing at all on four of its five call sites. Bound it once
        // here rather than at each of them: an out-of-range target then reaches the reader
        // and comes back refused, with an answer, instead of vanishing.
        const MAX_SEEK_SECONDS: f64 = 86_400.0;
        let latest_time = if latest_time.is_finite() {
            latest_time.clamp(0.0, MAX_SEEK_SECONDS)
        } else {
            0.0
        };

        self.pending_resume_seek = None;

        #[cfg(target_os = "windows")]
        {
            if self.is_asio_mode {
                if self.has_track {
                    // Dispatch first: announcing before a successful send would pin the UI on
                    // a target the decoder never received. Seeks in place and signals the
                    // control thread to flush the ring (ResetForSeek).
                    let gen_id = self.next_seek_gen();
                    let dispatched = self
                        .asio_seek_tx
                        .as_ref()
                        .is_some_and(|tx| tx.send((latest_time, gen_id)).is_ok());
                    if dispatched {
                        let target = latest_time.max(0.0);
                        self.last_asio_pos = Some(target);
                        // Pin the UI to the seek target until the decoder's ResetForSeek
                        // rebases the position (the control thread reports stale until then).
                        // Cleared by the matching SeekSettled.
                        self.seeking = true;
                        self.seek_target = Some(target);
                        (self.callback)(PlayerEvent::StateChange(
                            PlaybackState::Seeking,
                            self.current_seq,
                        ));
                        (self.callback)(PlayerEvent::TimeUpdate(target, self.current_seq));
                    } else {
                        // No live decoder to seek: report where playback actually is instead
                        // of the position the user asked for and will not be given.
                        let live = self.last_asio_pos.unwrap_or(0.0);
                        (self.callback)(PlayerEvent::TimeUpdate(live, self.current_seq));
                        crate::vprintln!("[SEEK]   asio: no live decoder; position restored");
                    }
                } else {
                    // No live decoder yet: queue as a user seek override, which
                    // supersedes any auto-resume the upcoming load resolves.
                    self.queue_user_seek(latest_time);
                }
                return;
            }
        }

        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                if self.has_track {
                    // Dispatch first: announcing before a successful send pins the UI on a
                    // target that never converges and nothing clears. Seeks in place (no
                    // respawn or re-probe) and signals the render to flush.
                    let gen_id = self.next_seek_gen();
                    let dispatched = self
                        .exclusive_seek_tx
                        .as_ref()
                        .is_some_and(|tx| tx.send((latest_time, gen_id)).is_ok());
                    if dispatched {
                        let target = latest_time.max(0.0);
                        // Cover the window before the decoder's first post-seek
                        // TimeUpdate (a back-to-shared re-arm may read this).
                        self.last_exclusive_pos = Some(target);
                        // Pin the UI to the target until the render's position converges, as
                        // the ASIO branch does: until the flush lands the backend still
                        // reports the pre-seek position, which would walk the bar back.
                        self.seeking = true;
                        self.seek_target = Some(target);
                        self.seek_wall_start = Some(std::time::Instant::now());
                        (self.callback)(PlayerEvent::StateChange(
                            PlaybackState::Seeking,
                            self.current_seq,
                        ));
                        (self.callback)(PlayerEvent::TimeUpdate(target, self.current_seq));
                    } else {
                        // No live decoder to seek: report where playback actually is instead
                        // of the position the user asked for and will not be given.
                        let live = self.last_exclusive_pos.unwrap_or(0.0);
                        (self.callback)(PlayerEvent::TimeUpdate(live, self.current_seq));
                        crate::vprintln!("[SEEK]   exclusive: no live decoder; position restored");
                    }
                } else {
                    // No live decoder yet: queue as a user seek override, which
                    // supersedes any auto-resume the upcoming load resolves.
                    self.queue_user_seek(latest_time);
                }
                return;
            }
        }

        if self.try_skip_pre_seek(latest_time) {
            // The pre-seek already put playback at the target, making it the position.
            if let Some(track_id) = self.current_track_id.as_ref() {
                self.resume_store.set(track_id, latest_time);
                self.resume_store.flush_if_due(false);
            }
            crate::vprintln!("[SEEK]   skipped (pre-seeked matches {:.2}s)", latest_time);
            return;
        }

        // Minted before the borrow below, as the two bypass branches above already do.
        let gen_id = self.next_seek_gen();
        if let Some(ref tx) = self.decode_cmd_tx {
            self.seeking = true;
            self.seek_target = Some(latest_time);
            self.seek_wall_start = Some(std::time::Instant::now());
            crate::state::GOVERNOR
                .buffer_progress()
                .request_seek_preload_pause();
            if let Some(ref m) = self.cpal_muted {
                m.store(true, Relaxed);
            }
            (self.callback)(PlayerEvent::StateChange(
                PlaybackState::Seeking,
                self.current_seq,
            ));
            (self.callback)(PlayerEvent::TimeUpdate(
                latest_time.max(0.0),
                self.current_seq,
            ));

            let _ = tx.send(DecodeCommand::Seek(latest_time, gen_id));
            crate::vprintln!(
                "[SEEK]   sent: {:.2}s ({})",
                latest_time,
                if self.is_cached {
                    "cached/RAM"
                } else {
                    "streaming"
                }
            );
        } else if self.has_track && self.loading_gen.is_none() {
            // The pipeline died (a fatal decode error nulls the sender); queuing is a lie
            // here, the next load overwrites the queue. Clear before setting: set() drops
            // anything at or under RESUME_MIN_SECONDS and would leave an older entry standing.
            let live = self.played_position_secs();
            if let Some(track_id) = self.current_track_id.clone() {
                self.resume_store.clear(&track_id);
                self.resume_store.set(&track_id, live);
                self.resume_store.flush_if_due(true);
            }
            (self.callback)(PlayerEvent::TimeUpdate(live, self.current_seq));
            crate::vprintln!("[SEEK]   no decoder and no load coming; position restored");
        } else {
            self.pending_resume_seek = Some(latest_time);
            crate::vprintln!("[SEEK]   queued until player ready");
        }
    }

    pub(super) fn handle_set_volume(&mut self, vol: crate::player::Volume) {
        // Provably finite and within 0..=100 by construction, which leaves the cast below
        // unable to saturate to infinity the way a raw f64 could.
        let vol_f32 = (vol.as_percent() / 100.0) as f32;
        // Record the real UI level on every path (incl. the session-sync Ok path
        // below, which pins self.volume to 1.0). The reliable seed for the
        // exclusive digital gain when volume_sync.get() later fails on switch.
        #[cfg(target_os = "windows")]
        self.last_volume.store(f32::to_bits(vol_f32), Relaxed);
        // Exclusive and ASIO bypass the OS session mixer; drive the shared digital
        // gain (the ASIO control thread reads the same cell as the WASAPI render).
        #[cfg(target_os = "windows")]
        if self.is_exclusive_mode || self.is_asio_mode {
            self.exclusive_gain.store(f32::to_bits(vol_f32), Relaxed);
        }
        #[cfg(target_os = "windows")]
        if let Some(ref vs) = self.volume_sync {
            match vs.set(vol_f32) {
                Ok(()) => {
                    self.volume.store(f32::to_bits(1.0), Relaxed);
                    return;
                }
                Err(_) => {
                    crate::vprintln!("[VOLUME] Session set failed, falling back to software gain");
                }
            }
            self.volume_sync = None;
            self.volume_rx = None;
        }
        self.volume.store(f32::to_bits(vol_f32), Relaxed);
    }

    #[cfg(target_os = "windows")]
    pub(super) fn init_volume_sync(&mut self) {
        if self._com_guard.is_none() || !self.volume_sync_enabled {
            return;
        }

        let device_id = self.current_device_id.as_deref().unwrap_or("default");

        let (tx, rx) = mpsc::channel();
        match crate::platform::volume_sync::VolumeSync::new(device_id, tx) {
            Ok(vs) => {
                if self.volume_baseline_established {
                    // Re-init (mode/device switch): assert our known level into the
                    // (possibly fresh) session instead of adopting its GetMasterVolume --
                    // a never-set session reports 1.0 by default (MS docs), which would
                    // poison last_volume to full scale and blast the exclusive/ASIO gain.
                    let app_vol = f32::from_bits(self.last_volume.load(Relaxed));
                    if let Err(e) = vs.set(app_vol) {
                        // Failed re-assert: do NOT trust the session (a never-set one
                        // sits at 1.0) and do NOT commit sync. Fall back to software
                        // gain at the app's level: same policy as the re-enable path
                        // in handle_set_volume_sync.
                        self.volume.store(f32::to_bits(app_vol), Relaxed);
                        crate::vprintln!(
                            "[VOLUME] Session re-assert failed: {e}; using software gain"
                        );
                        return;
                    }
                    self.volume.store(f32::to_bits(1.0), Relaxed);
                    crate::vprintln!(
                        "[VOLUME] Session sync re-asserted at {:.0}%",
                        app_vol * 100.0
                    );
                } else {
                    // Cold start: adopt the persisted session volume as the baseline and
                    // record it in last_volume directly (don't wait on the UI echo).
                    match vs.get() {
                        Ok(initial) => {
                            self.last_volume.store(f32::to_bits(initial), Relaxed);
                            self.volume_baseline_established = true;
                            let level = (initial * 100.0) as f64;
                            (self.callback)(PlayerEvent::VolumeSync(level));
                            self.volume.store(f32::to_bits(1.0), Relaxed);
                            crate::vprintln!(
                                "[VOLUME] Session sync active, initial level: {:.0}%",
                                level
                            );
                        }
                        Err(e) => {
                            crate::vprintln!(
                                "[VOLUME] Initial get failed: {e}, disabling OS volume sync"
                            );
                            return;
                        }
                    }
                }
                self.volume_sync = Some(vs);
                self.volume_rx = Some(rx);
            }
            Err(e) => {
                crate::vprintln!("[VOLUME] VolumeSync init failed: {e}, using software gain");
            }
        }
    }

    #[cfg(target_os = "windows")]
    pub(super) fn handle_set_volume_sync(&mut self, enabled: bool) {
        self.volume_sync_enabled = enabled;
        if enabled {
            if self.cpal_stream.is_some() && self._com_guard.is_some() {
                let app_vol = f32::from_bits(self.volume.load(Relaxed));
                let device_id = self.current_device_id.as_deref().unwrap_or("default");
                let (tx, rx) = mpsc::channel();
                match crate::platform::volume_sync::VolumeSync::new(device_id, tx) {
                    Ok(vs) => {
                        if let Err(e) = vs.set(app_vol) {
                            crate::vprintln!(
                                "[VOLUME] set failed on re-enable: {e}, staying on software gain"
                            );
                            return;
                        }
                        self.volume.store(f32::to_bits(1.0), Relaxed);
                        self.volume_sync = Some(vs);
                        self.volume_rx = Some(rx);
                        crate::vprintln!(
                            "[VOLUME] Session sync re-enabled at {:.0}%",
                            app_vol * 100.0
                        );
                    }
                    Err(e) => {
                        crate::vprintln!("[VOLUME] VolumeSync init failed on re-enable: {e}");
                    }
                }
            }
        } else if let Some(ref vs) = self.volume_sync {
            let level = match vs.get() {
                Ok(l) => l,
                Err(e) => {
                    crate::vprintln!("[VOLUME] Cannot disable sync: get() failed: {e}");
                    self.volume_sync_enabled = true;
                    return;
                }
            };
            // Mute cpal: the audio buffer has samples produced with software_gain=1.0.
            // Setting session to 1.0 would spike those to max. Muting lets at least
            // one callback drain the stale buffer before unmute (via mute_ack).
            if let Some(ref muted) = self.cpal_muted {
                muted.store(true, Relaxed);
            }
            self.volume.store(f32::to_bits(level), Relaxed);
            if let Err(e) = vs.set(1.0) {
                self.volume.store(f32::to_bits(1.0), Relaxed);
                if let Some(ref muted) = self.cpal_muted {
                    muted.store(false, Relaxed);
                }
                crate::vprintln!("[VOLUME] Cannot disable sync: set(1.0) failed: {e}");
                self.volume_sync_enabled = true;
                return;
            }
            if let Some(ref ack) = self.cpal_mute_ack {
                ack.store(false, Relaxed);
            }
            self.pending_unmute = true;
            self.volume_sync = None;
            self.volume_rx = None;
            crate::vprintln!(
                "[VOLUME] Session sync disabled, transferred {:.0}% to software gain",
                level * 100.0
            );
        }
    }

    pub(super) fn handle_get_audio_devices(&self, req_id: Option<String>) {
        let devices = super::output::enumerate_audio_devices();
        (self.callback)(PlayerEvent::AudioDevices(devices, req_id));
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/player/thread/commands.rs"]
mod tests;
