#[cfg(target_os = "windows")]
use super::commands::queued_seek_survives;
use super::decode::{DecodeThreadConfig, spawn_decode_thread};
use super::output::{
    format_duration_mmss, format_sample_rate, open_output_stream, probe_audio_format,
};
use super::{DecodeCommand, PlayerThread};
use crate::player::resume::RESUME_MIN_SECONDS;
use crate::player::{
    DeviceErrorKind, LoadRequest, MediaErrorCode, MediaFormatSnapshot, PlaybackState, PlayerEvent,
    ResumePolicy, current_gen, format_ms, short_id,
};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;

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

/// The queued seek names where the listener asked to be, the auto-resume only where they left
/// off. Every reader of the pair answers here; none can disagree with the one consuming it.
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

    /// The same answer, consuming neither candidate: a load announces where it will open
    /// before the backend that consumes the pair has been chosen.
    fn start_position(&self) -> Option<f64> {
        #[cfg(target_os = "windows")]
        let queued = self.user_seek_override.as_ref().map(|(_, time)| *time);
        #[cfg(not(target_os = "windows"))]
        let queued: Option<f64> = None;
        resolve_start_position(queued, self.pending_resume_seek)
    }

    /// Undo the commit this load made at entry. `committed_track` answers `Player::load`'s
    /// "same track?" on the caller thread, but `decide_play` keys on `has_track`, and after a
    /// failed load that flag still describes the track this one replaced: a re-assert minted
    /// before the failure then resumes a pipeline built from a buffer already cancelled here.
    fn abandon_failed_load(&mut self) {
        self.set_committed_track(None);
        self.has_track = false;
        // These two describe the track this load replaced, and every emitter that reads them
        // keys on the flag just cleared: left standing they answer for a track that is gone,
        // under the seq of the one that failed. `current_seq` stays. It names this load,
        // which any event emitted now belongs to.
        self.played_samples.store(0, Relaxed);
        self.current_duration = 0.0;
    }

    /// Announce a track that is loaded and silent. The bypass backends open their device on
    /// another thread, which costs seconds on a rate-locked interface; the frontend otherwise
    /// keeps the `active` it last had and runs its own clock over a stream yet to start.
    #[cfg(target_os = "windows")]
    fn announce_loaded(&self) {
        (self.callback)(PlayerEvent::StateChange(
            PlaybackState::Ready,
            self.current_seq,
        ));
    }

    pub(super) fn handle_load(
        &mut self,
        req: LoadRequest,
        #[cfg_attr(not(target_os = "windows"), expect(unused_variables))] auto_play: bool,
    ) -> bool {
        let LoadRequest {
            buffer,
            load_gen,
            seq: event_seq,
            track_id,
            product_id,
            resume_policy,
            load_start,
            cached,
            format,
        } = req;
        if load_gen != current_gen() {
            crate::vprintln!("[LOAD #{load_gen}] stale Load, ignoring");
            return false;
        }
        self.orphaned = false;
        // A load supersedes whatever was fading. Manual skip keeps today's cut. Everything that
        // follows sits under the gate on purpose: a Load this thread discards commits to nothing,
        // with no fade to supersede and no refusal to forget. The generation that retired it
        // may itself die to an HTTP error, and no failure path cancels a fade.
        self.cancel_crossfade();
        // A different track will stage a different next one; a past refusal
        // says nothing about it.
        self.xfade_refused_url = None;

        if let Some(ref prev) = self.current_track_id
            && *prev != track_id
        {
            self.resume_store.clear(prev);
        }

        self.current_track_id = Some(track_id.clone());
        self.current_product_id = product_id;
        self.set_committed_track(Some((track_id.clone(), format.clone())));
        self.pending_resume_seek = self.resolve_resume_policy(resume_policy, &track_id);
        self.current_seq = event_seq;
        #[cfg(target_os = "windows")]
        {
            self.load_seq = event_seq;
        }
        self.cached = cached;
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

        // `product` is on this line because the raw `player.load` args are redacted; an
        // absent id (which costs this track its length for good) is otherwise unobservable.
        crate::vprintln!(
            "[LOAD #{load_gen}] handle_load enter | cached={} | track={} | product={:?}",
            cached.is_some(),
            short_id(&track_id, 60),
            self.current_product_id.as_deref()
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
                self.governor.buffer_progress().set_playback_active(true);
                crate::vprintln!(
                    "[PLAY]   deferred play folded into ASIO start (load #{load_gen})"
                );
            }
            crate::vprintln!(
                "[ASIO] Progressive decode started ({:.0}ms setup)",
                decode_start.elapsed().as_secs_f64() * 1000.0
            );
            self.announce_loaded();
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
                self.governor.buffer_progress().set_playback_active(true);
                crate::vprintln!(
                    "[PLAY]   deferred play folded into exclusive start (load #{load_gen})"
                );
            }
            crate::vprintln!(
                "[WASAPI] Progressive decode started ({:.0}ms setup)",
                decode_start.elapsed().as_secs_f64() * 1000.0
            );
            self.announce_loaded();
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
                    seq: self.current_seq,
                    product_id: self.current_product_id.clone(),
                });
                // Settle the SDK's load() (its `mediaduration` await has no
                // timeout); 0, not the previous track's stale current_duration.
                (self.callback)(PlayerEvent::Duration(
                    0.0,
                    self.current_seq,
                    self.current_product_id.clone(),
                ));
                self.abandon_failed_load();
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

        // Open cpal stream
        let device = match self.resolve_output_device() {
            Some(d) => d,
            None => {
                self.abandon_failed_load();
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
                    self.abandon_failed_load();
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
        self.cpal_xfade = Some(opened.xfade);
        // A fresh stream starts its own generation count at zero.
        self.xfade_seen_gen = 0;

        self.sample_rate = actual_rate;
        self.channels = actual_channels;

        // Built only now: `self.sample_rate` reads the stream actually opened above, not the
        // previous track's rate (or the session's initial 44100) that stood here before the
        // device was touched. A load that fails to open returns above and emits no format
        // event, which beats announcing a format for a track that never opened.
        let media_format = MediaFormatSnapshot {
            codec: source_codec,
            sample_rate: source_sample_rate,
            output_sample_rate: self.sample_rate,
            bit_depth: source_bit_depth,
            channels: source_channels,
            bytes: total_len,
        };
        self.last_media_format = Some(media_format);
        (self.callback)(media_format.to_event());

        let (decode_cmd_tx, decode_cmd_rx) = mpsc::channel();
        let (decode_event_tx, decode_event_rx) = mpsc::channel();
        let decoded_samples = self.decoded_samples.clone();

        let decode_buffer = buffer.clone();
        let reader_cancel = Arc::new(AtomicBool::new(false));
        self.decode_reader_cancel = Some(reader_cancel.clone());
        let Some(decode_handle) = spawn_decode_thread(DecodeThreadConfig {
            buffer: decode_buffer,
            producer: ring_producer,
            decoded_samples,
            cmd_rx: decode_cmd_rx,
            event_tx: decode_event_tx,
            output_rate: actual_rate,
            output_channels: actual_channels,
            seek_gen,
            reader_cancel,
        }) else {
            // Same shape as the cpal-open failure above. The endpoint is open but nothing
            // will feed it; the load is abandoned rather than announced as Ready.
            (self.callback)(PlayerEvent::DeviceError(DeviceErrorKind::Unknown));
            self.abandon_failed_load();
            return false;
        };

        self.cpal_stream = Some(stream);
        self.decode_cmd_tx = Some(decode_cmd_tx);
        self.decode_event_rx = Some(decode_event_rx);
        self.decode_handle = Some(decode_handle);
        self.set_current_buffer(buffer);
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

        // Announced even when the container declares no length (0, the SDK's unknown sentinel): the
        // page learns which track Rust holds from this event, and its position, state and end are
        // taken for another track's until it does. The exclusive and ASIO backends report theirs
        // whatever it is.
        (self.callback)(PlayerEvent::Duration(
            self.current_duration,
            self.current_seq,
            self.current_product_id.clone(),
        ));

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
            let bp = self.governor.buffer_progress();
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
            if cached.is_some() {
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
        if generation == current_gen() {
            self.loading_gen = Some(generation);
        }
    }

    pub(super) fn handle_load_settled(&mut self, generation: u32) {
        let (loading_gen, pending_play) =
            settle_load(self.loading_gen, self.pending_play, generation);
        self.loading_gen = loading_gen;
        self.pending_play = pending_play;
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/player/thread/load.rs"]
mod tests;
