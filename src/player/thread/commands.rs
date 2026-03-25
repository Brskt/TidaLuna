use super::decode::{DecodeThreadConfig, spawn_decode_thread};
use super::output::{
    format_duration_mmss, format_sample_rate, open_output_stream, probe_audio_format,
};
use super::{DecodeCommand, PlayerThread};
use crate::player::resume::RESUME_MIN_SECONDS;
use crate::player::{
    DeviceErrorKind, LOAD_SEQ, LoadRequest, PlaybackState, PlayerCommand, PlayerEvent,
    ResumePolicy, format_ms,
};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;

use cpal::traits::{HostTrait, StreamTrait};

#[cfg(target_os = "windows")]
use crate::player::{EXCLUSIVE_STREAM_SEQ, wasapi};
#[cfg(target_os = "windows")]
use std::sync::Arc;
#[cfg(target_os = "windows")]
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "windows")]
use std::thread;
#[cfg(target_os = "windows")]
use wasapi::ExclusiveCommand;

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

    #[cfg(target_os = "windows")]
    pub(super) fn start_exclusive_playback(
        &mut self,
        buffer: crate::player::buffer::RamBuffer,
    ) -> bool {
        if !self.is_exclusive_mode {
            return false;
        }
        if let Some(ref handle) = self.exclusive_handle {
            handle.send(ExclusiveCommand::Stop);
            let cancel = Arc::new(AtomicBool::new(false));
            self.exclusive_stream_cancel = Some(cancel.clone());

            let cmd_tx = handle.command_sender();
            let reader = buffer.clone();
            let total_len = buffer.total_len();
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
                    eprintln!("[WASAPI] Stream decode failed: {e}");
                }
            });

            self.current_buffer = Some(buffer);
            self.has_track = true;
            self.is_playing = true;
        }
        true
    }

    pub(super) fn resolve_output_device(&self) -> Option<cpal::Device> {
        if let Some(ref id) = self.current_device_id {
            match super::output::find_output_device(id) {
                Some(d) => return Some(d),
                None => {
                    crate::vprintln!("[AUDIO] Device '{}' not found, falling back to default", id);
                    (self.callback)(PlayerEvent::DeviceError(DeviceErrorKind::NotFound));
                }
            }
        }
        match cpal::default_host().default_output_device() {
            Some(d) => Some(d),
            None => {
                (self.callback)(PlayerEvent::DeviceError(DeviceErrorKind::NotFound));
                None
            }
        }
    }

    pub(super) fn handle_load(&mut self, req: LoadRequest) {
        let LoadRequest {
            buffer,
            load_gen,
            seq: event_seq,
            track_id,
            resume_policy,
            load_start,
            cached,
        } = req;
        if load_gen != LOAD_SEQ.load(Relaxed) {
            crate::vprintln!("[LOAD #{load_gen}] stale Load, ignoring");
            return;
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
        self.buffer_stalled = false;
        self.pending_complete = false;
        self.last_played_snapshot = 0;

        crate::vprintln!(
            "[LOAD #{load_gen}] handle_load enter | cached={} | track={}",
            cached,
            track_id.chars().take(60).collect::<String>()
        );
        let handle_start = std::time::Instant::now();

        // Cancel previous playback
        #[cfg(target_os = "windows")]
        if let Some(cancel) = self.exclusive_stream_cancel.take() {
            cancel.store(true, Relaxed);
        }
        if let Some(ref old_buf) = self.current_buffer {
            old_buf.cancel();
        }
        self.stop_decode();

        let teardown_ms = handle_start.elapsed().as_secs_f64() * 1000.0;
        let decode_start = std::time::Instant::now();

        // WASAPI exclusive path
        #[cfg(target_os = "windows")]
        if self.start_exclusive_playback(buffer.clone()) {
            crate::vprintln!(
                "[WASAPI] Progressive decode started ({:.0}ms setup)",
                decode_start.elapsed().as_secs_f64() * 1000.0
            );
            return;
        }

        // Shared mode: symphonia + cpal
        let total_len = buffer.total_len();

        let probe = match probe_audio_format(&buffer) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[ERROR]  {e}");
                (self.callback)(PlayerEvent::MediaError {
                    error: e,
                    code: "mediaerror",
                });
                return;
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
            None => return,
        };

        let cpal_start = std::time::Instant::now();
        let opened =
            match open_output_stream(&device, source_sample_rate, source_channels, &self.volume) {
                Some(o) => o,
                None => {
                    (self.callback)(PlayerEvent::DeviceError(
                        DeviceErrorKind::FormatNotSupported,
                    ));
                    return;
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
    }

    pub(super) fn handle_play(&mut self) {
        self.allow_startup_auto_resume = false;

        if !self.has_track {
            crate::vprintln!("[PLAY]   ignored — no track loaded (has_track=false)");
            return;
        }

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
        self.start_playback();
    }

    pub(super) fn start_playback(&mut self) {
        if let Some(ref stream) = self.cpal_stream {
            match stream.play() {
                Ok(()) => crate::vprintln!("[PLAY]   cpal stream.play() OK"),
                Err(e) => eprintln!("[ERROR]  cpal stream.play() failed: {e}"),
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

        let pos_secs = self.current_position_secs();
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
        if let Some(cancel) = self.exclusive_stream_cancel.take() {
            cancel.store(true, Relaxed);
        }

        if let Some(ref old_buf) = self.current_buffer {
            old_buf.cancel();
        }
        self.stop_decode();
        self.current_buffer = None;

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
                self.current_duration = 0.0;
                self.current_track_id = None;
                self.pending_resume_seek = None;
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
        self.volume.store(f32::to_bits(vol_f32), Relaxed);
    }

    pub(super) fn handle_get_audio_devices(&self, req_id: Option<String>) {
        let devices = super::output::enumerate_audio_devices();
        (self.callback)(PlayerEvent::AudioDevices(devices, req_id));
    }
}
