use super::decode::{DecodeThreadConfig, spawn_decode_thread};
use super::output::{
    format_duration_mmss, format_sample_rate, open_output_stream, probe_audio_format,
};
use super::{DecodeCommand, PlayerThread};
use crate::player::resume::RESUME_MIN_SECONDS;
use crate::player::{
    DeviceErrorKind, LOAD_SEQ, LoadRequest, PlaybackState, PlayerCommand, PlayerEvent,
    ResumePolicy, format_ms, short_id,
};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;

use cpal::traits::StreamTrait;

#[cfg(target_os = "windows")]
use crate::player::asio::host::AsioCommand;
#[cfg(target_os = "windows")]
use crate::player::{ASIO_STREAM_SEQ, EXCLUSIVE_STREAM_SEQ, wasapi};
#[cfg(target_os = "windows")]
use std::sync::Arc;
#[cfg(target_os = "windows")]
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "windows")]
use std::thread;
#[cfg(target_os = "windows")]
use wasapi::ExclusiveCommand;

/// The defined outcomes of a `player.play`, given the player's current state.
/// Pure so it can be unit-tested without the audio pipeline.
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
/// deferred on it. Gen-matched, so a stale settle can't clear a newer load.
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

    pub(super) fn stop_decode(&mut self) {
        if let Some(tx) = self.decode_cmd_tx.take() {
            let _ = tx.send(DecodeCommand::Stop);
        }
        self.cpal_stream = None;
        if let Some(handle) = self.decode_handle.take() {
            let _ = handle.join();
        }
        self.decode_event_rx = None;
    }

    /// Spawn the exclusive decoder for `buffer`, seeking the source to `seek_to`.
    /// A fresh `stream_id` makes the render drop the prior decoder's stale
    /// PushPcm. No `Stop` (it emits `Stopped`, which clears the host-side track);
    /// the new `StartStream` is the reset.
    #[cfg(target_os = "windows")]
    fn spawn_exclusive_decoder(
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
            // Wake the retired reader so it sees the cancel and quiesces instead
            // of parking up to the read timeout while the new stream contends for
            // the same buffer (matches handle_set_audio_device).
            if let Some(ref buf) = self.current_buffer {
                buf.wake_readers();
            }
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.exclusive_stream_cancel = Some(cancel.clone());

        let (seek_tx, seek_rx) = mpsc::channel::<f64>();
        self.exclusive_seek_tx = Some(seek_tx);

        let reader = buffer.clone().with_reader_cancel(cancel.clone());
        let total_len = buffer.total_len();
        let stream_id = EXCLUSIVE_STREAM_SEQ.fetch_add(1, Relaxed) + 1;
        thread::spawn(move || {
            if let Err(e) = wasapi::stream_flac_reader_to_wasapi(
                reader,
                total_len,
                stream_id,
                cmd_tx,
                cancel.clone(),
                seek_to,
                start_paused,
                seek_rx,
            ) && !cancel.load(Relaxed)
            {
                crate::vprintln!("[WASAPI] Stream decode failed: {e}");
            }
        });

        self.current_buffer = Some(buffer);
        self.has_track = true;
        self.is_playing = !start_paused;
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
        // No ExclusiveCommand::Stop: its host-visible Stopped, polled after the
        // new StartStream, would null the just-loaded track on a playlist-advance.
        // The new StartStream re-bases the render instead. A queued user seek wins
        // over the load-time resume position.
        let seek_to = self
            .user_seek_override
            .take()
            .or_else(|| self.pending_resume_seek.take());
        self.spawn_exclusive_decoder(buffer, seek_to, start_paused);
        true
    }

    /// Spawn the ASIO decoder for `buffer`, seeking the source to `seek_to`. Mirrors
    /// `spawn_exclusive_decoder`: a fresh `stream_id` makes the control thread drop
    /// the prior decoder's stale `PushPcm`; the new `StartStream` is the reset.
    #[cfg(target_os = "windows")]
    fn spawn_asio_decoder(
        &mut self,
        buffer: crate::player::buffer::RamBuffer,
        seek_to: Option<f64>,
        start_paused: bool,
    ) {
        let Some((cmd_tx, buffered)) = self
            .asio_handle
            .as_ref()
            .map(|h| (h.command_sender(), h.buffered()))
        else {
            return;
        };
        if let Some(prev) = self.asio_stream_cancel.take() {
            prev.store(true, Relaxed);
            // Wake the retired reader so it quiesces instead of parking on the read
            // timeout while the new stream contends for the same buffer.
            if let Some(ref buf) = self.current_buffer {
                buf.wake_readers();
            }
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.asio_stream_cancel = Some(cancel.clone());

        let (seek_tx, seek_rx) = mpsc::channel::<f64>();
        self.asio_seek_tx = Some(seek_tx);

        let reader = buffer.clone().with_reader_cancel(cancel.clone());
        let stream_id = ASIO_STREAM_SEQ.fetch_add(1, Relaxed) + 1;
        // Record the live stream_id so poll_asio_events can reject stale Stopped/Completed
        // events from a superseded stream (otherwise they null a newer track -> double-load).
        self.current_asio_stream_id = Some(stream_id);
        // A fresh stream has no pending live seek; clear any stale seek-pin from a previous
        // track so its target can't pin this track's progress bar.
        self.seeking = false;
        self.seek_target = None;
        thread::spawn(move || {
            if let Err(e) = crate::player::asio::host::stream_reader_to_asio(
                reader,
                stream_id,
                cmd_tx,
                cancel.clone(),
                seek_to,
                start_paused,
                seek_rx,
                buffered,
            ) && !cancel.load(Relaxed)
            {
                crate::vprintln!("[ASIO] Stream decode failed: {e}");
            }
        });

        self.current_buffer = Some(buffer);
        self.has_track = true;
        self.is_playing = !start_paused;
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
        // A queued user seek wins over the load-time resume position.
        let seek_to = self
            .user_seek_override
            .take()
            .or_else(|| self.pending_resume_seek.take());
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
                if let Some(req) = self.current_device_id.as_deref()
                    && req != "auto"
                    && req != "default"
                    && name.as_deref() != Some(req)
                {
                    crate::vprintln!(
                        "[AUDIO] Device '{}' not found, falling back to default",
                        req
                    );
                }
                self.output_is_default = matches!(
                    self.current_device_id.as_deref(),
                    None | Some("auto") | Some("default")
                );
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
        self.pending_resume_seek = self.resolve_resume_policy(resume_policy, &track_id);
        self.current_seq = event_seq;
        self.is_cached = cached;
        self.current_format = format;
        self.buffer_stalled = false;
        self.pending_complete = false;
        self.last_played_snapshot = 0;

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
            // A new track invalidates any retained exclusive/asio position and any
            // user seek that was queued before the prior stream was ready.
            self.last_exclusive_pos = None;
            self.last_asio_pos = None;
            self.user_seek_override = None;
        }
        if let Some(ref old_buf) = self.current_buffer {
            old_buf.cancel();
        }
        self.stop_decode();

        let teardown_ms = handle_start.elapsed().as_secs_f64() * 1000.0;
        let decode_start = std::time::Instant::now();

        // ASIO path (mutually exclusive with WASAPI-exclusive). Same start_paused
        // contract; start_asio_playback consumes the resume position.
        #[cfg(target_os = "windows")]
        if self.start_asio_playback(buffer.clone(), !auto_play) {
            self.pending_play = None;
            crate::vprintln!(
                "[ASIO] Progressive decode started ({:.0}ms setup)",
                decode_start.elapsed().as_secs_f64() * 1000.0
            );
            return true;
        }

        // Exclusive path. start_paused = !auto_play (a paused restore enters
        // paused); start_exclusive_playback consumes the resume position.
        #[cfg(target_os = "windows")]
        if self.start_exclusive_playback(buffer.clone(), !auto_play) {
            // When auto_play, exclusive auto-starts playback (is_playing=true),
            // which satisfies any deferred play; clear it so it can't dangle.
            self.pending_play = None;
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
                    code: "mediaerror",
                });
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

        (self.callback)(PlayerEvent::MediaFormat {
            codec: source_codec,
            sample_rate: source_sample_rate,
            bit_depth: source_bit_depth,
            channels: source_channels,
            bytes: total_len,
        });

        // Open cpal stream
        let device = match self.resolve_output_device() {
            Some(d) => d,
            None => return false,
        };

        let cpal_start = std::time::Instant::now();
        let opened =
            match open_output_stream(&device, source_sample_rate, source_channels, &self.volume) {
                Some(o) => o,
                None => {
                    (self.callback)(PlayerEvent::DeviceError(
                        DeviceErrorKind::FormatNotSupported,
                    ));
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

        // Volume sync: only init once - rebinding at each track causes drift because
        // the PID-based session lookup can pick a stale/wrong session during transitions.
        // Re-init happens on device switch (device.rs) or toggle (handle_set_volume_sync).
        #[cfg(target_os = "windows")]
        if self.volume_sync.is_none() {
            self.init_volume_sync();
        }

        // Pre-seek
        self.pre_seek_pos = None;
        if let Some(pos) = self.pending_resume_seek
            && let Some(ref tx) = self.decode_cmd_tx
        {
            let _ = tx.send(DecodeCommand::Seek(pos));
            self.pre_seek_pos = Some(pos);
            crate::vprintln!("[LOAD #{load_gen}] pre-seek to {:.1}s (decode paused)", pos);
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

        // Honor a play that raced ahead of this load. Tagged by load_gen so a
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
                    (self.callback)(PlayerEvent::ReplayRequest {
                        track,
                        expected_gen: LOAD_SEQ.load(Relaxed),
                        position: None,
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
                if let Some(seek_time) = self
                    .user_seek_override
                    .take()
                    .or_else(|| self.pending_resume_seek.take())
                {
                    self.last_asio_pos = Some(seek_time.max(0.0));
                    if let Some(ref tx) = self.asio_seek_tx {
                        let _ = tx.send(seek_time);
                    } else if let Some(buffer) = self.current_buffer.clone() {
                        self.spawn_asio_decoder(buffer, Some(seek_time), false);
                    }
                    (self.callback)(PlayerEvent::TimeUpdate(seek_time, self.current_seq));
                } else if let Some(ref handle) = self.asio_handle {
                    handle.send(AsioCommand::Play);
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
                if let Some(seek_time) = self
                    .user_seek_override
                    .take()
                    .or_else(|| self.pending_resume_seek.take())
                {
                    // Seek queued before the stream was ready: send to the live
                    // decoder, else spawn at the position. Record last_exclusive_pos
                    // for a back-to-shared re-arm before the first TimeUpdate.
                    self.last_exclusive_pos = Some(seek_time.max(0.0));
                    if let Some(ref tx) = self.exclusive_seek_tx {
                        let _ = tx.send(seek_time);
                    } else if let Some(buffer) = self.current_buffer.clone() {
                        self.spawn_exclusive_decoder(buffer, Some(seek_time), false);
                    }
                    (self.callback)(PlayerEvent::TimeUpdate(seek_time, self.current_seq));
                } else if let Some(ref handle) = self.exclusive_handle {
                    handle.send(ExclusiveCommand::Play);
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

        if let Some(pos) = self.pending_resume_seek.take() {
            (self.callback)(PlayerEvent::TimeUpdate(pos.max(0.0), self.current_seq));
            crate::vprintln!("[PLAY]   start at resume {:.1}s (pre-seeked)", pos);
        } else {
            crate::vprintln!("[PLAY]   start from beginning");
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
        if let Some(ref stream) = self.cpal_stream {
            match stream.play() {
                Ok(()) => crate::vprintln!("[PLAY]   cpal stream.play() OK"),
                Err(e) => crate::vprintln!("[ERROR]  cpal stream.play() failed: {e}"),
            }
        } else {
            eprintln!("[ERROR]  start_playback: no cpal stream!");
        }
        if let Some(ref tx) = self.decode_cmd_tx {
            let _ = tx.send(DecodeCommand::Resume);
            crate::vprintln!("[PLAY]   DecodeCommand::Resume sent");
        } else {
            eprintln!("[ERROR]  start_playback: no decode_cmd_tx!");
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
        #[cfg(target_os = "windows")]
        {
            if self.is_asio_mode {
                if let Some(ref handle) = self.asio_handle {
                    handle.send(AsioCommand::Pause);
                }
                self.is_playing = false;
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
        self.current_seq = event_seq;
        crate::state::GOVERNOR.reset_buffer_progress();

        #[cfg(target_os = "windows")]
        {
            if let Some(cancel) = self.exclusive_stream_cancel.take() {
                cancel.store(true, Relaxed);
            }
            self.exclusive_seek_tx = None;
            self.last_exclusive_pos = None;
            if let Some(cancel) = self.asio_stream_cancel.take() {
                cancel.store(true, Relaxed);
            }
            self.asio_seek_tx = None;
            self.last_asio_pos = None;
        }

        if let Some(ref old_buf) = self.current_buffer {
            old_buf.cancel();
        }
        self.stop_decode();
        self.current_buffer = None;

        #[cfg(target_os = "windows")]
        {
            if self.is_asio_mode {
                if let Some(ref handle) = self.asio_handle {
                    handle.send(AsioCommand::Stop);
                }
                (self.callback)(PlayerEvent::TimeUpdate(0.0, self.current_seq));
                (self.callback)(PlayerEvent::StateChange(
                    PlaybackState::Stopped,
                    self.current_seq,
                ));
                self.is_playing = false;
                self.has_track = false;
                self.pending_play = None;
                self.loading_gen = None;
                self.current_duration = 0.0;
                self.current_track_id = None;
                self.pending_resume_seek = None;
                self.user_seek_override = None;
                self.pre_seek_pos = None;
                self.resume_store.flush_if_due(true);
                return;
            }
        }

        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                if let Some(ref handle) = self.exclusive_handle {
                    handle.send(ExclusiveCommand::Stop);
                }
                (self.callback)(PlayerEvent::TimeUpdate(0.0, self.current_seq));
                (self.callback)(PlayerEvent::StateChange(
                    PlaybackState::Stopped,
                    self.current_seq,
                ));
                self.is_playing = false;
                self.has_track = false;
                self.pending_play = None;
                self.loading_gen = None;
                self.current_duration = 0.0;
                self.current_track_id = None;
                self.pending_resume_seek = None;
                self.user_seek_override = None;
                self.pre_seek_pos = None;
                self.resume_store.flush_if_due(true);
                return;
            }
        }

        (self.callback)(PlayerEvent::TimeUpdate(0.0, self.current_seq));
        (self.callback)(PlayerEvent::StateChange(
            PlaybackState::Stopped,
            self.current_seq,
        ));
        self.is_playing = false;
        self.has_track = false;
        self.pending_play = None;
        self.loading_gen = None;
        self.current_duration = 0.0;
        self.current_track_id = None;
        self.pending_resume_seek = None;
        self.pre_seek_pos = None;
        self.is_cached = false;
        self.buffer_stalled = false;
        self.resume_store.flush_if_due(true);
    }

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

        if let Some(track_id) = self.current_track_id.as_ref() {
            self.resume_store.set(track_id, latest_time);
            self.resume_store.flush_if_due(false);
        }
        self.pending_resume_seek = None;

        #[cfg(target_os = "windows")]
        {
            if self.is_asio_mode {
                if self.has_track {
                    self.last_asio_pos = Some(latest_time.max(0.0));
                    // Pin the UI to the seek target until the decoder's ResetForSeek rebases
                    // the position (the control thread reports stale until then). Cleared in
                    // poll_asio_events on convergence.
                    self.seeking = true;
                    self.seek_target = Some(latest_time.max(0.0));
                    // Seek the live decoder in place: it format.seek()s and signals
                    // the control thread to flush the ring (ResetForSeek).
                    if let Some(ref tx) = self.asio_seek_tx {
                        let _ = tx.send(latest_time);
                    }
                    (self.callback)(PlayerEvent::TimeUpdate(
                        latest_time.max(0.0),
                        self.current_seq,
                    ));
                } else {
                    self.user_seek_override = Some(latest_time);
                }
                return;
            }
        }

        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                if self.has_track {
                    // Cover the window before the decoder's first post-seek
                    // TimeUpdate (a back-to-shared re-arm may read this).
                    self.last_exclusive_pos = Some(latest_time.max(0.0));
                    // Seek the live decoder in place (no respawn / re-probe):
                    // it format.seek()s and signals the render to flush.
                    if let Some(ref tx) = self.exclusive_seek_tx {
                        let _ = tx.send(latest_time);
                    }
                    (self.callback)(PlayerEvent::TimeUpdate(
                        latest_time.max(0.0),
                        self.current_seq,
                    ));
                } else {
                    // No live decoder yet: queue as a user seek override so it
                    // supersedes any auto-resume the upcoming load resolves.
                    self.user_seek_override = Some(latest_time);
                }
                return;
            }
        }

        if self.try_skip_pre_seek(latest_time) {
            crate::vprintln!("[SEEK]   skipped (pre-seeked matches {:.2}s)", latest_time);
            return;
        }

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

            let _ = tx.send(DecodeCommand::Seek(latest_time));
            crate::vprintln!(
                "[SEEK]   sent: {:.2}s ({})",
                latest_time,
                if self.is_cached {
                    "cached/RAM"
                } else {
                    "streaming"
                }
            );
        } else {
            self.pending_resume_seek = Some(latest_time);
            crate::vprintln!("[SEEK]   queued until player ready");
        }
    }

    pub(super) fn handle_set_volume(&mut self, vol: f64) {
        let vol_f32 = (vol / 100.0) as f32;
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
                match vs.get() {
                    Ok(initial) => {
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
mod tests {
    use super::{PlayAction, decide_play, settle_load};

    #[test]
    fn play_with_live_track_resumes() {
        assert_eq!(decide_play(true, None, false), PlayAction::Resume);
        assert_eq!(decide_play(true, Some(7), true), PlayAction::Resume);
    }

    #[test]
    fn play_while_loading_defers_to_that_generation() {
        assert_eq!(decide_play(false, Some(7), true), PlayAction::DeferTo(7));
        // a load in flight wins even when a retained source also exists
        assert_eq!(decide_play(false, Some(3), false), PlayAction::DeferTo(3));
    }

    #[test]
    fn play_with_no_load_but_retained_source_rearms() {
        assert_eq!(decide_play(false, None, true), PlayAction::ReArm);
    }

    #[test]
    fn play_cold_empty_is_ignored() {
        assert_eq!(decide_play(false, None, false), PlayAction::Ignore);
    }

    #[test]
    fn settle_clears_loading_for_matching_gen() {
        assert_eq!(settle_load(Some(5), None, 5), (None, None));
    }

    #[test]
    fn settle_ignores_a_mismatched_gen() {
        // a stale settle for an old gen must not clear a newer in-flight load
        assert_eq!(settle_load(Some(6), None, 5), (Some(6), None));
    }

    #[test]
    fn settle_clears_a_deferred_play_waiting_on_a_failed_load() {
        // the load failed (no handle_load delivered), so the deferred play
        // tagged with that gen must not dangle
        assert_eq!(settle_load(Some(5), Some(5), 5), (None, None));
    }

    #[test]
    fn settle_leaves_a_deferred_play_for_a_different_gen() {
        assert_eq!(settle_load(None, Some(9), 5), (None, Some(9)));
    }
}
