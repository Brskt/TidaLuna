use super::output::{STREAM_ERR_DEVICE_LOST, STREAM_ERR_NONE, STREAM_ERR_UNKNOWN};
use super::{DecodeEvent, PlayerThread};
use crate::player::{DeviceErrorKind, MediaErrorCode, PlaybackState, PlayerEvent, format_ms};
use std::sync::atomic::Ordering::Relaxed;

#[cfg(target_os = "windows")]
use crate::player::asio::host::{AsioCommand, AsioEvent};
#[cfg(target_os = "windows")]
use crate::player::wasapi::{ExclusiveCommand, ExclusiveEvent};

/// Whether a fatal `DecodeEvent::Error` must be settled in place: the drain path
/// only takes over after a trailing `Finished` and with at least one sample
/// played - otherwise nothing else ever emits a terminal state. Pure, to
/// be unit-tested without the audio pipeline.
fn decode_failure_needs_settle(pending_complete: bool, played_samples: u64) -> bool {
    !pending_complete || played_samples == 0
}

/// Whether a backend's settlement answers the seek the player is actually waiting on.
/// `stream_id` alone only proves the decoder was not replaced; two seeks issued on one
/// live stream share it, leaving the older answer free to settle the newer. Pure, to be
/// unit-tested without a backend. Windows-only: only the bypass backends carry an ack.
#[cfg(target_os = "windows")]
fn seek_ack_is_current(
    current_stream_id: Option<u32>,
    current_gen: u32,
    ack_stream_id: u32,
    ack_gen: u32,
) -> bool {
    current_stream_id == Some(ack_stream_id) && current_gen == ack_gen
}

/// Whether a per-track device verdict may be charged to the track now playing. Two ids match
/// or nothing does: a refusal can take seconds, and a superseded stream's would otherwise mark
/// whatever track loaded while it negotiated. Pure, to be unit-tested without a backend.
#[cfg(target_os = "windows")]
fn verdict_names_current_stream(current: Option<u32>, verdict: Option<u32>) -> bool {
    matches!((current, verdict), (Some(a), Some(b)) if a == b)
}

impl<F: Fn(PlayerEvent) + Send + 'static> PlayerThread<F> {
    pub(super) fn samples_to_secs(&self, samples: u64) -> f64 {
        let channels = self.channels.max(1) as u64;
        let frames = samples / channels;
        frames as f64 / self.sample_rate.max(1) as f64
    }

    pub(super) fn played_position_secs(&self) -> f64 {
        self.samples_to_secs(self.played_samples.load(Relaxed))
    }

    /// Position to report or rebuild at: the pending seek target while a seek is in
    /// flight (played_samples isn't rebased until SeekComplete), else the played position.
    pub(super) fn effective_position(&self) -> f64 {
        self.seek_target
            .unwrap_or_else(|| self.played_position_secs())
    }

    /// The state a settled seek puts back: `is_playing` covers Active, `idle_state` the rest.
    /// Only a synchronous transport command may write `idle_state`, since a backend's own
    /// echo arrives late enough to let a pause's ack overwrite a stop that superseded it.
    pub(super) fn settled_state(&self) -> PlaybackState {
        if self.is_playing {
            PlaybackState::Active
        } else {
            self.idle_state
        }
    }

    /// A bypass decoder died mid-stream: the SDK is told why and left on a terminal state.
    /// The resume point is deliberately kept, since the listener never reached the end.
    #[cfg(target_os = "windows")]
    pub(super) fn settle_bypass_decode_failure(&mut self, error: String) {
        // Silence the backend and arm its release. Clearing bookkeeping is half of what
        // `stop_decode()` does for the shared path; it also drops the cpal stream. Here the
        // ring still holds DECODE_AHEAD_SECS of audio, and no EndStream means no completion.
        if self.is_exclusive_mode {
            if let Some(ref handle) = self.exclusive_handle {
                match self.current_exclusive_stream_id {
                    Some(stream_id) => handle.send(ExclusiveCommand::Pause { stream_id }),
                    None => crate::vprintln!("[WASAPI] decode failure: no live stream to silence"),
                }
            }
            // The debounced block checks for a handle itself: a timer set without one no-ops.
            self.exclusive_release_at =
                Some(std::time::Instant::now() + super::EXCLUSIVE_PAUSE_RELEASE);
        }
        if self.is_asio_mode {
            if let Some(ref handle) = self.asio_handle {
                match self.current_asio_stream_id {
                    Some(stream_id) => handle.send(AsioCommand::Pause { stream_id }),
                    None => crate::vprintln!("[ASIO]   decode failure: no live stream to silence"),
                }
            }
            self.asio_release_at = Some(std::time::Instant::now() + super::ASIO_IDLE_RELEASE);
        }
        // Cached bytes that will not decode must not stay indexed: every later play would
        // fail the same way, and the load path just refreshed their LRU position.
        if self.is_cached
            && let Some(tid) = self.current_track_id.clone()
            && let Ok(mut cache) = crate::state::AUDIO_CACHE.lock()
            && cache.drop_entry(&tid) == crate::player::cache::DropOutcome::Dropped
        {
            crate::vprintln!("[CACHE]  Dropped after a decode failure: {tid}");
        }
        (self.callback)(PlayerEvent::MediaError {
            error,
            code: MediaErrorCode::UnreadableFile,
        });
        // The SDK's same-track recovery reload rebuilds instead of resuming a dead decoder.
        self.set_committed_track(None);
        self.has_track = false;
        self.is_playing = false;
        self.seeking = false;
        self.seek_target = None;
        self.seek_wall_start = None;
        self.exclusive_seek_tx = None;
        self.asio_seek_tx = None;
        self.current_track_id = None;
        self.idle_state = PlaybackState::Stopped;
        crate::state::GOVERNOR
            .buffer_progress()
            .set_playback_active(false);
        (self.callback)(PlayerEvent::StateChange(
            PlaybackState::Stopped,
            self.current_seq,
        ));
    }

    /// Where playback actually is, asked of the backend that owns it. `played_samples` (the
    /// cpal counter) sits frozen at the last shared position while ASIO or exclusive is
    /// engaged, since neither bypass backend writes it. `None` before anything played.
    #[cfg(target_os = "windows")]
    pub(super) fn live_position_secs(&self) -> Option<f64> {
        if self.is_asio_mode {
            return self.last_asio_pos;
        }
        if self.is_exclusive_mode {
            return self.last_exclusive_pos;
        }
        let shared = self.effective_position();
        (shared > 0.0).then_some(shared)
    }

    #[cfg(target_os = "windows")]
    pub(super) fn poll_exclusive_events(&mut self) {
        if self.is_exclusive_mode {
            // Settled after the loop, outside the handle borrow: the settle takes `&mut
            // self`, where the arms below only touch fields.
            let mut decode_failure: Option<String> = None;
            if let Some(ref handle) = self.exclusive_handle {
                for ev in handle.poll_events() {
                    match ev {
                        ExclusiveEvent::SeekSettled {
                            stream_id,
                            gen_id,
                            position,
                            refused,
                        } => {
                            // Both halves of the identity are needed: the stream rejects an ack
                            // from a replaced decoder, the generation rejects an older sibling
                            // seek's ack on that same decoder.
                            if seek_ack_is_current(
                                self.current_exclusive_stream_id,
                                self.seek_ack_gen,
                                stream_id,
                                gen_id,
                            ) {
                                self.seeking = false;
                                self.seek_target = None;
                                self.seek_wall_start = None;
                                if let Some(track_id) = self.current_track_id.clone() {
                                    // Clear first, accepted or refused: `set` drops anything at
                                    // or under RESUME_MIN_SECONDS, which leaves an older and
                                    // later entry standing.
                                    self.resume_store.clear(&track_id);
                                    self.resume_store.set(&track_id, position);
                                    self.resume_store.flush_if_due(refused);
                                }
                                self.last_exclusive_pos = Some(position);
                                // handle_seek announced Seeking; its end is owed.
                                (self.callback)(PlayerEvent::StateChange(
                                    self.settled_state(),
                                    self.current_seq,
                                ));
                                (self.callback)(PlayerEvent::TimeUpdate(
                                    position,
                                    self.current_seq,
                                ));
                            }
                        }
                        ExclusiveEvent::TimeUpdate(t) => {
                            // While a seek is pending the render still reports the pre-seek
                            // position (not rebased until the flush lands); pin the UI to the
                            // target. Only SeekSettled ends the pin: distance from the target
                            // cannot tell a refused seek from a stale report, which is what
                            // left the pin held for the rest of the track.
                            let report = match self.seek_target {
                                Some(target) if self.seeking => target,
                                _ => t,
                            };
                            // Only a settled seek writes a seek position: the pin is a target no
                            // backend has accepted, and a pause arriving mid-seek forces a flush
                            // that would put it on disk.
                            if !self.seeking
                                && let Some(track_id) = self.current_track_id.as_ref()
                            {
                                self.resume_store.set(track_id, report);
                                self.resume_store.flush_if_due(false);
                            }
                            // Floor-free live position for an exclusive->shared re-arm.
                            self.last_exclusive_pos = Some(report);
                            (self.callback)(PlayerEvent::TimeUpdate(report, self.current_seq));
                        }
                        ExclusiveEvent::DecodeFailed { stream_id, error } => {
                            if self.current_exclusive_stream_id == Some(stream_id) {
                                crate::vprintln!("[WASAPI] decoder died: {error}");
                                decode_failure = Some(error);
                            }
                        }
                        ExclusiveEvent::StateChange(s) => {
                            // Transport states only. Completion arrives named, below: clearing
                            // the track is the one effect a superseded stream must never have,
                            // and this arm cannot tell whose stream it is.
                            (self.callback)(PlayerEvent::StateChange(s, self.current_seq));
                        }
                        ExclusiveEvent::Completed(sid) => {
                            // Ignore a completion from a superseded stream (a newer track
                            // already loaded): only the current stream's completion clears
                            // the track; a stale one would force a spurious re-arm/double-load.
                            if self.current_exclusive_stream_id == Some(sid) {
                                if let Some(track_id) = self.current_track_id.as_ref() {
                                    self.resume_store.clear(track_id);
                                    self.resume_store.flush_if_due(true);
                                }
                                self.has_track = false;
                                self.is_playing = false;
                                // A seek in flight when the track ended can no longer be
                                // answered: its sender dies below, and the pin would hold the
                                // command loop at its 1ms seeking cadence until the next load.
                                self.seeking = false;
                                self.seek_target = None;
                                self.seek_wall_start = None;
                                self.current_track_id = None;
                                self.set_committed_track(None);
                                self.last_exclusive_pos = None;
                                // Decoder exited (EndStream), its seek receiver
                                // dropped: clear the dead sender; a later
                                // seek/play respawns a decoder.
                                self.exclusive_seek_tx = None;
                                crate::state::GOVERNOR
                                    .buffer_progress()
                                    .set_playback_active(false);
                                (self.callback)(PlayerEvent::StateChange(
                                    PlaybackState::Completed,
                                    self.current_seq,
                                ));
                            } else {
                                // Logged, unlike the ASIO twin's silent drop: its own
                                // `FormatUnsupported`/`RateUnsupported` siblings name the
                                // superseded stream, and a completion earns the same.
                                crate::vprintln!(
                                    "[WASAPI] completion for superseded stream {}; current is {:?}",
                                    sid,
                                    self.current_exclusive_stream_id
                                );
                            }
                        }
                        ExclusiveEvent::Duration { stream_id, secs } => {
                            // Forwarded only for the stream still owned: a superseded stream
                            // measured another track.
                            if verdict_names_current_stream(
                                self.current_exclusive_stream_id,
                                Some(stream_id),
                            ) {
                                self.current_duration = secs;
                                (self.callback)(PlayerEvent::Duration(secs, self.current_seq));
                            }
                        }
                        ExclusiveEvent::InitFailed(e) => {
                            crate::vprintln!(
                                "[WASAPI] Init failed, falling back to shared mode: {e}"
                            );
                            if let Some(cancel) = self.exclusive_stream_cancel.take() {
                                cancel.store(true, Relaxed);
                            }
                            self.is_exclusive_mode = false;
                            (self.callback)(PlayerEvent::DeviceError(
                                DeviceErrorKind::ExclusiveModeNotAllowed,
                            ));
                            self.rearm_shared_after_exclusive_failure();
                        }
                        ExclusiveEvent::DeviceLocked(e) => {
                            crate::vprintln!("[WASAPI] Device locked by another process: {e}");
                            if let Some(cancel) = self.exclusive_stream_cancel.take() {
                                cancel.store(true, Relaxed);
                            }
                            self.is_exclusive_mode = false;
                            (self.callback)(PlayerEvent::DeviceError(DeviceErrorKind::Locked));
                            self.rearm_shared_after_exclusive_failure();
                        }
                        ExclusiveEvent::FormatUnsupported { stream_id } => {
                            if let Some(cancel) = self.exclusive_stream_cancel.take() {
                                cancel.store(true, Relaxed);
                            }
                            // Per-track skip: remember this track; its shared re-arm must not
                            // loop back into exclusive, but keep exclusive on globally
                            // (ExclusiveFormatUnsupported is not in the frontend disable list).
                            // Only the stream still owned may be marked: the refusal took the
                            // render thread down whichever stream it judged; the re-arm below
                            // is owed either way, but the mark belongs to the refused track.
                            if verdict_names_current_stream(
                                self.current_exclusive_stream_id,
                                Some(stream_id),
                            ) {
                                crate::vprintln!(
                                    "[WASAPI] device can't do this track's format in exclusive; this track plays shared (exclusive stays on for other rates)"
                                );
                                self.exclusive_skip_track = self.current_track_id.clone();
                            } else {
                                crate::vprintln!(
                                    "[WASAPI] format refused for superseded stream {stream_id} (current {:?}); re-arming shared without marking the live track",
                                    self.current_exclusive_stream_id
                                );
                            }
                            self.is_exclusive_mode = false;
                            (self.callback)(PlayerEvent::DeviceError(
                                DeviceErrorKind::ExclusiveFormatUnsupported,
                            ));
                            self.rearm_shared_after_exclusive_failure();
                        }
                    }
                }
            }

            if let Some(error) = decode_failure {
                self.settle_bypass_decode_failure(error);
            }

            if !self.is_exclusive_mode {
                self.exclusive_handle = None;
            }

            // Debounced release: once a pause has lingered past EXCLUSIVE_PAUSE_RELEASE
            // without a resume or track change, hand the exclusive device back, letting
            // other apps regain it. has_track=false + committed cleared makes the next play
            // re-arm (decide_play ReArm) or a same-track reload reopen (no idempotent
            // skip); both reopen the client.
            if self.is_exclusive_mode
                && !self.is_playing
                && let Some(at) = self.exclusive_release_at
                && std::time::Instant::now() >= at
            {
                self.exclusive_release_at = None;
                if let Some(ref handle) = self.exclusive_handle {
                    handle.send(ExclusiveCommand::ReleaseDevice);
                }
                self.has_track = false;
                self.set_committed_track(None);
                crate::vprintln!(
                    "[WASAPI] Released exclusive device on stop (other apps can use it)"
                );
            }
        }
    }

    #[cfg(target_os = "windows")]
    pub(super) fn poll_asio_events(&mut self) {
        if !self.is_asio_mode {
            return;
        }
        // Settled after the loop, outside the handle borrow: the settle takes `&mut self`,
        // where the arms below only touch fields.
        let mut decode_failure: Option<String> = None;
        if let Some(ref handle) = self.asio_handle {
            for ev in handle.poll_events() {
                match ev {
                    AsioEvent::SeekSettled {
                        stream_id,
                        gen_id,
                        position,
                        refused,
                    } => {
                        // Both halves of the identity are needed: the stream rejects an ack
                        // from a decoder this thread has replaced, the generation rejects an
                        // older sibling seek's ack on that same decoder. Without the second,
                        // a stale settle re-anchors the watchdog below on a position that
                        // has stopped moving, and 2s later ASIO ejects the track for good.
                        if seek_ack_is_current(
                            self.current_asio_stream_id,
                            self.seek_ack_gen,
                            stream_id,
                            gen_id,
                        ) {
                            self.seeking = false;
                            self.seek_target = None;
                            self.seek_wall_start = None;
                            if let Some(track_id) = self.current_track_id.clone() {
                                // Clear first, accepted or refused: `set` drops anything at or
                                // under RESUME_MIN_SECONDS, which leaves an older and later
                                // entry standing.
                                self.resume_store.clear(&track_id);
                                self.resume_store.set(&track_id, position);
                                self.resume_store.flush_if_due(refused);
                            }
                            self.last_asio_pos = Some(position);
                            // A >=2s decoder-blocked seek landing on the pinned target trips a
                            // false RateUnsupported, hence the re-anchor. Only while the clock
                            // runs: paused, a settle proves nothing and the anchor reads as a
                            // stall on the first Play.
                            if self.is_playing {
                                self.asio_watchdog_pos = position;
                                self.asio_watchdog_at = Some(std::time::Instant::now());
                            }
                            // handle_seek announced Seeking; its end is owed.
                            (self.callback)(PlayerEvent::StateChange(
                                self.settled_state(),
                                self.current_seq,
                            ));
                            (self.callback)(PlayerEvent::TimeUpdate(position, self.current_seq));
                        }
                    }
                    AsioEvent::TimeUpdate(t) => {
                        // While a seek is pending the control thread still reports the stale
                        // position (not rebased until ResetForSeek lands); pin the UI to the
                        // target. Only SeekSettled ends the pin: distance from the target
                        // cannot tell a refused seek from a stale report, which is what left
                        // the pin held for the rest of the track.
                        let report = match self.seek_target {
                            Some(target) if self.seeking => target,
                            _ => t,
                        };
                        // Only a settled seek writes a seek position: the pin is a target no
                        // backend has accepted, and a pause arriving mid-seek forces a flush
                        // that would put it on disk.
                        if !self.seeking
                            && let Some(track_id) = self.current_track_id.as_ref()
                        {
                            self.resume_store.set(track_id, report);
                            self.resume_store.flush_if_due(false);
                        }
                        // Floor-free live position for an asio->shared re-arm.
                        self.last_asio_pos = Some(report);
                        // Watchdog: any position change (forward progress or a seek landing)
                        // proves the clock ticks -> reset the stall anchor.
                        if (report - self.asio_watchdog_pos).abs() > 0.05 {
                            self.asio_watchdog_pos = report;
                            self.asio_watchdog_at = Some(std::time::Instant::now());
                        }
                        (self.callback)(PlayerEvent::TimeUpdate(report, self.current_seq));
                    }
                    AsioEvent::DecodeFailed { stream_id, error } => {
                        if self.current_asio_stream_id == Some(stream_id) {
                            crate::vprintln!("[ASIO] decoder died: {error}");
                            decode_failure = Some(error);
                        }
                    }
                    AsioEvent::StateChange(s) => {
                        // Arm the progress watchdog when the clock starts; disarm on pause/stop.
                        if matches!(s, PlaybackState::Active) {
                            self.asio_watchdog_pos = self.last_asio_pos.unwrap_or(0.0);
                            self.asio_watchdog_at = Some(std::time::Instant::now());
                        } else if matches!(s, PlaybackState::Paused | PlaybackState::Stopped) {
                            self.asio_watchdog_at = None;
                        }
                        (self.callback)(PlayerEvent::StateChange(s, self.current_seq));
                    }
                    AsioEvent::Completed(sid) => {
                        // Ignore a completion from a superseded stream (a newer track
                        // already loaded): only the current stream's completion clears
                        // the track; a stale one would force a spurious re-arm/double-load.
                        if self.current_asio_stream_id == Some(sid) {
                            if let Some(track_id) = self.current_track_id.as_ref() {
                                self.resume_store.clear(track_id);
                                self.resume_store.flush_if_due(true);
                            }
                            self.has_track = false;
                            self.is_playing = false;
                            // A seek in flight when the track ended can no longer be answered:
                            // its sender dies below, and the pin would hold the command loop at
                            // its 1ms seeking cadence until the next load.
                            self.seeking = false;
                            self.seek_target = None;
                            self.seek_wall_start = None;
                            self.current_track_id = None;
                            self.set_committed_track(None);
                            self.last_asio_pos = None;
                            self.asio_watchdog_at = None;
                            // Decoder exited (EndStream): clear the dead sender, and a
                            // later seek/play respawns a decoder.
                            self.asio_seek_tx = None;
                            crate::state::GOVERNOR
                                .buffer_progress()
                                .set_playback_active(false);
                            (self.callback)(PlayerEvent::StateChange(
                                PlaybackState::Completed,
                                self.current_seq,
                            ));
                        }
                    }
                    AsioEvent::Duration { stream_id, secs } => {
                        // Same scoping as the exclusive twin.
                        if verdict_names_current_stream(
                            self.current_asio_stream_id,
                            Some(stream_id),
                        ) {
                            self.current_duration = secs;
                            (self.callback)(PlayerEvent::Duration(secs, self.current_seq));
                        }
                    }
                    AsioEvent::DriverNotFound => {
                        crate::vprintln!(
                            "[ASIO] No ASIO driver found, falling back to shared mode"
                        );
                        if let Some(cancel) = self.asio_stream_cancel.take() {
                            cancel.store(true, Relaxed);
                        }
                        self.is_asio_mode = false;
                        (self.callback)(PlayerEvent::DeviceError(
                            DeviceErrorKind::AsioDriverNotFound,
                        ));
                        self.rearm_shared_after_asio_failure();
                    }
                    AsioEvent::FormatUnsupported { stream_id } => {
                        // Scoped like `RateUnsupported` below: the channel-count half of this
                        // refusal reads the TRACK's channels, so a superseded verdict would
                        // cancel a live decoder over a count the driver was never asked about.
                        // The sample-type half is the driver's own, and the next build re-reports it.
                        if verdict_names_current_stream(self.current_asio_stream_id, stream_id) {
                            crate::vprintln!(
                                "[ASIO] Driver rejects the track format, falling back to shared"
                            );
                            if let Some(cancel) = self.asio_stream_cancel.take() {
                                cancel.store(true, Relaxed);
                            }
                            self.is_asio_mode = false;
                            (self.callback)(PlayerEvent::DeviceError(
                                DeviceErrorKind::AsioFormatUnsupported,
                            ));
                            self.rearm_shared_after_asio_failure();
                        } else {
                            crate::vprintln!(
                                "[ASIO] format refused for superseded stream {stream_id:?} (current {:?}); the live track keeps ASIO",
                                self.current_asio_stream_id
                            );
                        }
                    }
                    AsioEvent::RateUnsupported { stream_id } => {
                        // Scoped whole, like `Completed` and `DecodeFailed` above, and unlike the
                        // exclusive twin. There the refusal takes the one render thread down
                        // whichever stream it judged, so its re-arm is owed either way; no ASIO
                        // refusal does that. `finish_rebuild` and the reset give-up leave the
                        // control thread alive on `Idle`, so acting on a superseded verdict would
                        // cancel a live decoder and demote a track the driver never refused.
                        if verdict_names_current_stream(self.current_asio_stream_id, stream_id) {
                            if let Some(cancel) = self.asio_stream_cancel.take() {
                                cancel.store(true, Relaxed);
                            }
                            // Per-track skip: remember this track; its shared re-arm must NOT
                            // re-engage ASIO (loop), but keep ASIO on globally (no sticky clear:
                            // `AsioRateUnsupported` is not in the frontend's disable list).
                            crate::vprintln!(
                                "[ASIO] device can't clock this track's rate; this track plays shared (ASIO stays on for other rates)"
                            );
                            self.asio_skip_track = self.current_track_id.clone();
                            self.is_asio_mode = false;
                            (self.callback)(PlayerEvent::DeviceError(
                                DeviceErrorKind::AsioRateUnsupported,
                            ));
                            self.rearm_shared_after_asio_failure();
                        } else {
                            crate::vprintln!(
                                "[ASIO] rate refused for superseded stream {stream_id:?} (current {:?}); the live track keeps ASIO",
                                self.current_asio_stream_id
                            );
                        }
                    }
                    AsioEvent::InitFailed(e) => {
                        crate::vprintln!("[ASIO] Init failed, falling back to shared mode: {e}");
                        if let Some(cancel) = self.asio_stream_cancel.take() {
                            cancel.store(true, Relaxed);
                        }
                        self.is_asio_mode = false;
                        (self.callback)(PlayerEvent::DeviceError(DeviceErrorKind::AsioInitFailed));
                        self.rearm_shared_after_asio_failure();
                    }
                }
            }
        }

        if let Some(error) = decode_failure {
            self.settle_bypass_decode_failure(error);
        }

        // Progress watchdog (a backstop): the clock reported Active but the position hasn't
        // advanced within the timeout -> the driver can't clock this track. Route it to shared
        // as a PER-TRACK skip (RateUnsupported), NOT a hard failure: a single un-clockable
        // track must never auto-disable ASIO for the whole session.
        if self.is_asio_mode
            && self.is_playing
            && !self.seeking
            && self
                .asio_watchdog_at
                .is_some_and(|a| a.elapsed() >= std::time::Duration::from_secs(2))
        {
            crate::vprintln!(
                "[ASIO] watchdog: no playback progress for 2s; this track plays shared (ASIO stays on)"
            );
            if let Some(cancel) = self.asio_stream_cancel.take() {
                cancel.store(true, Relaxed);
            }
            // Mirror the RateUnsupported exit: remember the track; its shared re-arm must not
            // loop back into ASIO, and emit AsioRateUnsupported (not in the frontend's disable
            // list) to keep ASIO enabled.
            self.asio_skip_track = self.current_track_id.clone();
            self.is_asio_mode = false;
            self.asio_watchdog_at = None;
            (self.callback)(PlayerEvent::DeviceError(
                DeviceErrorKind::AsioRateUnsupported,
            ));
            self.rearm_shared_after_asio_failure();
        }

        // Debounced idle release (sustained pause or terminal stop): free the
        // ASIO driver, letting other apps regain it. loading_gen blocks a slow
        // in-flight stop->load (a failed load settles it via LoadSettleGuard and
        // the release then proceeds). has_track=false + committed cleared route
        // the next play/load through the full rebuild path (start_asio_playback
        // respawns the handle). Gated on a live handle: a stale timer surviving
        // a parked mode switch must not clobber the retained-track state.
        if self.is_asio_mode
            && !self.is_playing
            && self.loading_gen.is_none()
            && self.asio_handle.is_some()
            && let Some(at) = self.asio_release_at
            && std::time::Instant::now() >= at
        {
            self.asio_release_at = None;
            // Cancel the detached decoder first: its wait sites never observe the
            // control thread dying: it would leak (thread + full-track buffer).
            if let Some(cancel) = self.asio_stream_cancel.take() {
                cancel.store(true, Relaxed);
                if let Some(ref buf) = self.current_buffer {
                    buf.wake_readers();
                }
            }
            self.asio_seek_tx = None;
            if let Some(handle) = self.asio_handle.take() {
                self.asio_teardown = handle.shutdown();
            }
            self.has_track = false;
            self.set_committed_track(None);
            crate::vprintln!("[ASIO]   Released idle driver (other apps can use it)");
        }

        if !self.is_asio_mode {
            self.asio_handle = None;
        }
    }

    /// Reap a drained ASIO teardown and run the device switch parked behind
    /// it. The switch fires whenever the teardown slot is empty, no matter
    /// who reaped it (a load's bounded reap must not strand it), and bypasses
    /// the idempotent guards: is_asio_mode stays true for the whole drain.
    #[cfg(target_os = "windows")]
    pub(super) fn poll_asio_teardown(&mut self) {
        if self.asio_teardown.as_ref().is_some_and(|h| h.is_finished())
            && let Some(handle) = self.asio_teardown.take()
        {
            let _ = handle.join();
            crate::vprintln!("[ASIO] teardown: reaped");
        }
        if self.asio_teardown.is_none()
            && let Some((id, mode)) = self.pending_device_switch.take()
        {
            crate::vprintln!("[ASIO] teardown: parked switch re-dispatched");
            self.apply_device_switch(id, mode);
        }
    }

    /// Bounded wait for a parked teardown, for paths that must respawn the
    /// driver NOW (self-heal load). False on timeout: the driver is wedged,
    /// do not double-open it.
    #[cfg(target_os = "windows")]
    pub(super) fn reap_asio_teardown_within(&mut self, timeout: std::time::Duration) -> bool {
        let Some(handle) = self.asio_teardown.take() else {
            return true;
        };
        let deadline = std::time::Instant::now() + timeout;
        while !handle.is_finished() {
            if std::time::Instant::now() >= deadline {
                self.asio_teardown = Some(handle);
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let _ = handle.join();
        true
    }

    /// ASIO failed: re-arm a fresh shared load at the live position; the current track
    /// recovers immediately (the custom `deviceasio*` events aren't TIDAL-native, and the
    /// frontend won't re-arm). Mirrors the device.rs asio->shared switch. No-op without a track.
    #[cfg(target_os = "windows")]
    fn rearm_shared_after_asio_failure(&mut self) {
        let was_playing = self.is_playing;
        self.has_track = false;
        self.is_playing = false;
        self.loading_gen = None;
        self.pending_play = None;
        self.seeking = false;
        self.seek_target = None;
        self.asio_seek_tx = None;

        // Prefer the live ASIO position (floor-free); fall back to resume_store.
        let track = crate::state::CURRENT_TRACK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let position = self.last_asio_pos.or_else(|| {
            track.as_ref().and_then(|t| {
                self.resume_store
                    .get(&crate::player::canonical_track_id(&t.url))
            })
        });
        self.last_asio_pos = None;
        if let Some(track) = track {
            (self.callback)(PlayerEvent::ReplayRequest {
                track,
                expected_gen: crate::player::LOAD_SEQ.load(Relaxed),
                position,
                play: was_playing,
            });
        } else {
            // Non-replayable source (DASH): no track to re-arm, and playback stops (as in the
            // device.rs switch paths). Logged to keep the silent stop diagnosable.
            crate::vprintln!(
                "[ASIO] fallback: non-replayable source, playback stopped (cannot re-arm shared)"
            );
        }
    }

    /// Exclusive WASAPI failed (OS denied exclusive, or the device is locked): re-arm a
    /// fresh shared load at the live position; the current track keeps playing instead
    /// of stopping. Mirrors `rearm_shared_after_asio_failure`. No-op without a track.
    #[cfg(target_os = "windows")]
    fn rearm_shared_after_exclusive_failure(&mut self) {
        let was_playing = self.is_playing;
        self.has_track = false;
        self.is_playing = false;
        self.loading_gen = None;
        self.pending_play = None;
        self.seeking = false;
        self.seek_target = None;
        self.exclusive_seek_tx = None;

        // Prefer the live exclusive position (floor-free); fall back to resume_store.
        let track = crate::state::CURRENT_TRACK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let position = self.last_exclusive_pos.or_else(|| {
            track.as_ref().and_then(|t| {
                self.resume_store
                    .get(&crate::player::canonical_track_id(&t.url))
            })
        });
        self.last_exclusive_pos = None;
        if let Some(track) = track {
            (self.callback)(PlayerEvent::ReplayRequest {
                track,
                expected_gen: crate::player::LOAD_SEQ.load(Relaxed),
                position,
                play: was_playing,
            });
        } else {
            crate::vprintln!(
                "[WASAPI] exclusive fallback: non-replayable source, playback stopped (cannot re-arm shared)"
            );
        }
    }

    pub(super) fn poll_playback(&mut self) {
        #[cfg(target_os = "windows")]
        let should_poll = !self.is_exclusive_mode && !self.is_asio_mode;
        #[cfg(not(target_os = "windows"))]
        let should_poll = true;

        // Update governor buffer progress whenever a track is loaded, in EVERY output mode:
        // the decode thread reads the RamBuffer in shared, exclusive, and ASIO alike: the
        // governor's read_pos must track it regardless of `should_poll`. Gating it off froze
        // read_pos; the governor then saw an ever-growing `ahead`, paused the download, and
        // never resumed, starving the decoder into silence.
        if self.has_track
            && let Some(ref buf) = self.current_buffer
        {
            let bp = crate::state::GOVERNOR.buffer_progress();
            bp.written.store(buf.written(), Relaxed);
            bp.read_pos.store(buf.read_cursor(), Relaxed);
        }

        // Detect cpal stream errors and recover from device loss.
        let stream_err_code = self
            .cpal_stream_error
            .as_ref()
            .map_or(STREAM_ERR_NONE, |flag| flag.swap(STREAM_ERR_NONE, Relaxed));
        if stream_err_code == STREAM_ERR_DEVICE_LOST {
            (self.callback)(PlayerEvent::DeviceError(DeviceErrorKind::Disconnected));
            // Device lost / invalidated: rebuild on the current default device.
            self.recover_audio_device();
        } else if stream_err_code == STREAM_ERR_UNKNOWN {
            (self.callback)(PlayerEvent::DeviceError(DeviceErrorKind::Unknown));
        }

        // The decode thread answers a seek while paused: it dequeues the command, seeks, and
        // reports SeekComplete. Draining its channel cannot wait for playback to resume.
        if !should_poll || !self.has_track {
            return;
        }

        // Detect decode thread stalling on RamBuffer (buffering). Read before the drain
        // below, letting a seek landing this tick still count as in flight: a range restart
        // always blocks the reader, and that stall is the seek's own doing.
        if self.is_playing
            && !self.is_cached
            && !self.seeking
            && let Some(ref buf) = self.current_buffer
        {
            let stalled = buf.is_stalled();
            if stalled && !self.buffer_stalled {
                self.buffer_stalled = true;
                (self.callback)(PlayerEvent::StateChange(
                    PlaybackState::Idle,
                    self.current_seq,
                ));
            } else if !stalled && self.buffer_stalled {
                self.buffer_stalled = false;
                (self.callback)(PlayerEvent::StateChange(
                    PlaybackState::Active,
                    self.current_seq,
                ));
            }
        }

        let mut fatal_decode = false;
        if let Some(ref rx) = self.decode_event_rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    DecodeEvent::Finished => {
                        crate::vprintln!(
                            "[TRACK]  Decode finished ({})",
                            if self.is_cached {
                                "from cache"
                            } else {
                                "from stream"
                            }
                        );
                        // Store the ciphertext the download staged. The decoded bytes stay
                        // in the buffer: taking them would empty it and cost the track its
                        // reusability (see RamBuffer::is_reusable).
                        if !self.is_cached
                            && let Some(ref buf) = self.current_buffer
                            && buf.is_complete()
                            && let Some((staged, len)) = buf.take_ciphertext()
                        {
                            let track_id = self.current_track_id.clone();
                            let cache_format = self.current_format.clone();
                            std::thread::spawn(move || {
                                let Some(tid) = track_id else { return };
                                // Resolve the path under a brief lock, then write
                                // the multi-MB file unlocked; a concurrent
                                // track-load lookup is not blocked behind it.
                                let (path, store_gen) = {
                                    let Ok(cache) = crate::state::AUDIO_CACHE.lock() else {
                                        crate::vprintln!("[CACHE]  Lock poisoned, skipping store");
                                        return;
                                    };
                                    (cache.file_path(&tid), cache.generation())
                                };
                                if let Err(e) =
                                    crate::player::cache::AudioCache::persist_file(&path, staged)
                                {
                                    crate::vprintln!("[CACHE]  Store failed (persist): {e}");
                                    return;
                                }
                                // Index insert + eviction under a short lock,
                                // skipped (and the file removed) if a cache clear
                                // raced this unlocked write.
                                let Ok(mut cache) = crate::state::AUDIO_CACHE.lock() else {
                                    crate::vprintln!("[CACHE]  Lock poisoned, skipping index");
                                    return;
                                };
                                use crate::player::cache::StoreOutcome;
                                match cache.record_if_current(&tid, &cache_format, len, store_gen) {
                                    Ok(StoreOutcome::Kept) => crate::vprintln!(
                                        "[CACHE]  Stored: {} ({}, encrypted)",
                                        tid,
                                        crate::player::format_bytes(len)
                                    ),
                                    // Reported with both sizes, ungated, by the cache itself.
                                    Ok(StoreOutcome::TooLarge) => {}
                                    // Ungated (disk held over the cap, no other channel), and
                                    // retried only by a later eviction pass or store of this
                                    // id.
                                    Ok(StoreOutcome::TooLargeRetained) => crate::verr!(
                                        "[CACHE]  Oversized entry could not be removed, indexed at {}: {}",
                                        crate::player::format_bytes(len),
                                        tid
                                    ),
                                    // Nothing was staged; nothing to report.
                                    Ok(StoreOutcome::Disabled) => {}
                                    Ok(StoreOutcome::ClearedMidWrite) => crate::vprintln!(
                                        "[CACHE]  Store discarded (cache cleared mid-write): {tid}"
                                    ),
                                    Err(e) => {
                                        crate::vprintln!("[CACHE]  Store failed (index): {e}")
                                    }
                                }
                            });
                        }

                        self.pending_complete = true;
                        self.last_played_snapshot =
                            self.played_samples.load(Relaxed).wrapping_sub(1);
                        crate::vprintln!(
                            "[DRAIN]  Waiting for ring buffer drain (decoded={}, played={}, duration={:.2}s)",
                            self.decoded_samples.load(Relaxed),
                            self.played_samples.load(Relaxed),
                            self.current_duration,
                        );
                    }
                    DecodeEvent::SeekComplete {
                        gen_id,
                        position,
                        refused,
                    } => {
                        // A superseded seek's answer changes nothing here. Unmuting on one made
                        // the ring audible while a newer seek was still on its way.
                        if gen_id != self.seek_ack_gen {
                            continue;
                        }
                        // Only a decoder parked at EOF leaves a completion pending here, since
                        // Finished arms it once nothing is left to decode. The mute freezes the
                        // played count the drain check reads, which would take this for the end.
                        self.pending_complete = false;
                        // Ring accounting, not position: this wants where decode stands now,
                        // not `position`. Only an accepted seek flushed the ring and rebased
                        // the decoder onto the landing point. A refusal moved nothing, and
                        // rebasing on a decoder a whole ring ahead teleports the played count.
                        if !refused {
                            self.played_samples
                                .store(self.decoded_samples.load(Relaxed), Relaxed);
                        }
                        if let Some(ref m) = self.cpal_muted {
                            m.store(false, Relaxed);
                        }
                        if self.seeking {
                            self.seeking = false;
                            self.seek_target = None;
                            // A refusal reports the decode cursor, which runs ahead of what was
                            // heard; only the played counter answers where the listener is. The
                            // two bypass backends build their own refusals on that same counter.
                            let settled = if refused {
                                self.played_position_secs()
                            } else {
                                position
                            };
                            // The settle is the only writer of a seek position on this path:
                            // the periodic tick below returns early while playback is stopped,
                            // and a seek taken in pause would otherwise never reach disk.
                            if let Some(track_id) = self.current_track_id.as_ref() {
                                self.resume_store.clear(track_id);
                                self.resume_store.set(track_id, settled);
                                self.resume_store.flush_if_due(refused);
                            }
                            let wall_ms = self
                                .seek_wall_start
                                .take()
                                .map(|s| s.elapsed().as_secs_f64() * 1000.0)
                                .unwrap_or(0.0);
                            crate::vprintln!(
                                "[SEEK]   complete: wall={} ({})",
                                format_ms(wall_ms),
                                if self.is_cached {
                                    "cached"
                                } else {
                                    "streaming"
                                }
                            );
                            // handle_seek announced Seeking over the state it interrupted; that
                            // state goes back here.
                            (self.callback)(PlayerEvent::StateChange(
                                self.settled_state(),
                                self.current_seq,
                            ));
                        } else if refused {
                            // A load-time pre-seek never arms the pin; its answer lands here.
                            // The marker was a bet placed when that seek was sent, and a refusal
                            // means nothing reached it. Left standing, it lets the follow-up seek
                            // skip its own dispatch and persist a position never played.
                            self.pre_seek_pos = None;
                        } else {
                            // Accepted: the marker becomes where the reader actually landed, which
                            // is the figure the skip decision has to compare a later seek against.
                            self.pre_seek_pos = Some(position);
                        }
                    }
                    DecodeEvent::Error(e) => {
                        crate::vprintln!("[DECODE] Error: {e}");
                        // Cached bytes that will not decode must not stay indexed: the load
                        // path already refreshed their LRU position: nothing else would
                        // ever retire them and every later play would fail the same way.
                        // Re-downloading one good track is the cheap side of that trade.
                        if self.is_cached
                            && let Some(tid) = self.current_track_id.clone()
                            && let Ok(mut cache) = crate::state::AUDIO_CACHE.lock()
                            && cache.drop_entry(&tid) == crate::player::cache::DropOutcome::Dropped
                        {
                            crate::vprintln!("[CACHE]  Dropped after a decode failure: {tid}");
                        }
                        (self.callback)(PlayerEvent::MediaError {
                            error: e,
                            code: MediaErrorCode::UnreadableFile,
                        });
                        // Init-time decode errors emit Error with no trailing Finished,
                        // and nothing else clears committed_track. Drop it here for the SDK's
                        // same-track recovery reload to rebuild instead of resuming a dead decoder.
                        self.set_committed_track(None);
                        // Every site that reports an error returns right after: the channel
                        // is dead: left Some, a later seek arms a guard nothing can clear.
                        self.decode_cmd_tx = None;
                        // Settled below, outside the rx borrow.
                        fatal_decode = true;
                    }
                }
            }
        }

        // Without a settle the SDK stays stranded on its last reported state.
        // Stopped maps to NOT_PLAYING (the SDK's failed-to-resume contract), and
        // stop_decode() keeps a later seek from muting against a dead channel.
        // Mid-stream errors with audio played keep the drain->Completed path.
        // A paused player never reaches that drain path, and a dead decoder can no longer
        // answer a seek in flight: settle here, or the media error arrives with no terminal
        // state behind it and the seek pin never clears.
        if fatal_decode
            && (decode_failure_needs_settle(
                self.pending_complete,
                self.played_samples.load(Relaxed),
            ) || !self.is_playing
                || self.seeking)
        {
            self.stop_decode();
            self.pending_complete = false;
            self.last_played_snapshot = 0;
            self.seeking = false;
            self.seek_target = None;
            self.seek_wall_start = None;
            self.has_track = false;
            self.is_playing = false;
            self.current_track_id = None;
            self.current_duration = 0.0;
            crate::state::GOVERNOR
                .buffer_progress()
                .set_playback_active(false);
            self.idle_state = PlaybackState::Stopped;
            (self.callback)(PlayerEvent::StateChange(
                PlaybackState::Stopped,
                self.current_seq,
            ));
            return;
        }

        // The drain check below compares played_samples against its own previous snapshot;
        // a paused stream freezes that counter, and the track then reads as completed.
        if !self.is_playing {
            return;
        }

        // A seek in flight mutes cpal, which freezes that same counter, and the listener
        // asked for another position: the track is not over.
        if self.pending_complete && !self.seeking {
            let played = self.played_samples.load(Relaxed);
            if played > 0 && played == self.last_played_snapshot {
                crate::vprintln!(
                    "[DRAIN]  Ring buffer drained (played={}, decoded={})",
                    played,
                    self.decoded_samples.load(Relaxed),
                );
                self.pending_complete = false;
                self.last_played_snapshot = 0;
                // A played-out track owns no resume position, and a same-URL reload must
                // rebuild rather than resume in place. Decode EOF arrives up to a ring buffer
                // early; a pause there would commit both against an unfinished track.
                if let Some(track_id) = self.current_track_id.as_ref() {
                    self.resume_store.clear(track_id);
                    self.resume_store.flush_if_due(true);
                }
                self.set_committed_track(None);
                (self.callback)(PlayerEvent::TimeUpdate(
                    self.current_duration,
                    self.current_seq,
                ));
                (self.callback)(PlayerEvent::StateChange(
                    PlaybackState::Completed,
                    self.current_seq,
                ));
                // The decode thread parks at EOF rather than exiting; completion retires it.
                // Left alone it holds a live command channel, and a seek on a finished track
                // revives it to decode for nobody. The fatal-error settle above does the same.
                self.stop_decode();
                self.has_track = false;
                self.is_playing = false;
                self.current_track_id = None;
                crate::state::GOVERNOR
                    .buffer_progress()
                    .set_playback_active(false);
                self.current_duration = 0.0;
                return;
            }
            self.last_played_snapshot = played;
        }

        // Emit time update
        if let Some(target) = self.seek_target {
            // Re-entered every poll tick with a frozen target the flush dedupes.
            if self.last_seek_emit != Some(target) {
                self.last_seek_emit = Some(target);
                (self.callback)(PlayerEvent::TimeUpdate(target, self.current_seq));
            }
        } else {
            self.last_seek_emit = None;
            let pos_secs = self.played_position_secs();
            let pos_secs = if self.current_duration > 0.0 {
                pos_secs.min(self.current_duration)
            } else {
                pos_secs
            };
            if pos_secs > 0.0 {
                // Don't re-write resume_store while the track is completing: after decode
                // EOF (Finished already cleared it) the ring is still draining, and this tick
                // would otherwise re-store a near-duration position. That stale entry then
                // survives into a replay and seeks the next mode-switch straight to EOF
                // (which auto-advances to the next track).
                if !self.pending_complete
                    && let Some(track_id) = self.current_track_id.as_ref()
                {
                    self.resume_store.set(track_id, pos_secs);
                    self.resume_store.flush_if_due(false);
                }
                (self.callback)(PlayerEvent::TimeUpdate(pos_secs, self.current_seq));
            }
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/player/thread/playback.rs"]
mod tests;
