use super::output::{STREAM_ERR_DEVICE_LOST, STREAM_ERR_NONE, STREAM_ERR_UNKNOWN};
use super::{DecodeEvent, PlayerThread};
use crate::player::{DeviceErrorKind, MediaErrorCode, PlaybackState, PlayerEvent, format_ms};
use std::sync::atomic::Ordering::Relaxed;

#[cfg(target_os = "windows")]
use crate::player::asio::host::AsioEvent;
#[cfg(target_os = "windows")]
use crate::player::wasapi::{ExclusiveCommand, ExclusiveEvent};

/// Whether a fatal `DecodeEvent::Error` must be settled in place: the drain path
/// only takes over after a trailing `Finished` and with at least one sample
/// played - otherwise nothing else ever emits a terminal state. Pure so it can
/// be unit-tested without the audio pipeline.
fn decode_failure_needs_settle(pending_complete: bool, played_samples: u64) -> bool {
    !pending_complete || played_samples == 0
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

    #[cfg(target_os = "windows")]
    pub(super) fn poll_exclusive_events(&mut self) {
        if self.is_exclusive_mode {
            if let Some(ref handle) = self.exclusive_handle {
                for ev in handle.poll_events() {
                    match ev {
                        ExclusiveEvent::TimeUpdate(t) => {
                            if let Some(track_id) = self.current_track_id.as_ref() {
                                self.resume_store.set(track_id, t);
                                self.resume_store.flush_if_due(false);
                            }
                            // Floor-free live position for an exclusive->shared re-arm.
                            self.last_exclusive_pos = Some(t);
                            (self.callback)(PlayerEvent::TimeUpdate(t, self.current_seq));
                        }
                        ExclusiveEvent::StateChange(s) => {
                            if s == PlaybackState::Completed {
                                if let Some(track_id) = self.current_track_id.as_ref() {
                                    self.resume_store.clear(track_id);
                                    self.resume_store.flush_if_due(true);
                                }
                                self.has_track = false;
                                self.is_playing = false;
                                self.current_track_id = None;
                                self.set_committed_track(None);
                                self.last_exclusive_pos = None;
                                // Decoder exited (EndStream), its seek receiver
                                // dropped: clear the dead sender so a later
                                // seek/play respawns a decoder.
                                self.exclusive_seek_tx = None;
                                crate::state::GOVERNOR
                                    .buffer_progress()
                                    .set_playback_active(false);
                            }
                            (self.callback)(PlayerEvent::StateChange(s, self.current_seq));
                        }
                        ExclusiveEvent::Duration(d) => {
                            self.current_duration = d;
                            (self.callback)(PlayerEvent::Duration(d, self.current_seq));
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
                        ExclusiveEvent::FormatUnsupported => {
                            crate::vprintln!(
                                "[WASAPI] device can't do this track's format in exclusive; this track plays shared (exclusive stays on for other rates)"
                            );
                            if let Some(cancel) = self.exclusive_stream_cancel.take() {
                                cancel.store(true, Relaxed);
                            }
                            // Per-track skip: remember this track so its shared re-arm doesn't
                            // loop back into exclusive, but keep exclusive on globally
                            // (ExclusiveFormatUnsupported is not in the frontend disable list).
                            self.exclusive_skip_track = self.current_track_id.clone();
                            self.is_exclusive_mode = false;
                            (self.callback)(PlayerEvent::DeviceError(
                                DeviceErrorKind::ExclusiveFormatUnsupported,
                            ));
                            self.rearm_shared_after_exclusive_failure();
                        }
                    }
                }
            }

            if !self.is_exclusive_mode {
                self.exclusive_handle = None;
            }

            // Debounced release: once a pause has lingered past EXCLUSIVE_PAUSE_RELEASE
            // without a resume or track change, hand the exclusive device back so other
            // apps regain it. has_track=false + committed cleared makes the next play
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
        if let Some(ref handle) = self.asio_handle {
            for ev in handle.poll_events() {
                match ev {
                    AsioEvent::TimeUpdate(t) => {
                        // While a live seek is pending the control thread still reports the
                        // stale position (not rebased until ResetForSeek lands); pin the UI to
                        // the target until `t` converges so the bar doesn't flicker back.
                        let report = match self.seek_target {
                            Some(target) if self.seeking => {
                                if (t - target).abs() <= 1.0 {
                                    self.seeking = false;
                                    self.seek_target = None;
                                    // Convergence IS progress: re-anchor the stall watchdog.
                                    // A >=2s decoder-blocked seek that lands exactly on the
                                    // pinned target would miss the 0.05 refresh below and
                                    // trip a false RateUnsupported in this very poll.
                                    self.asio_watchdog_pos = t;
                                    self.asio_watchdog_at = Some(std::time::Instant::now());
                                    t
                                } else {
                                    target
                                }
                            }
                            _ => t,
                        };
                        if let Some(track_id) = self.current_track_id.as_ref() {
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
                        // already loaded) -- only the current stream's completion clears
                        // the track; a stale one would force a spurious re-arm/double-load.
                        if self.current_asio_stream_id == Some(sid) {
                            if let Some(track_id) = self.current_track_id.as_ref() {
                                self.resume_store.clear(track_id);
                                self.resume_store.flush_if_due(true);
                            }
                            self.has_track = false;
                            self.is_playing = false;
                            self.current_track_id = None;
                            self.set_committed_track(None);
                            self.last_asio_pos = None;
                            self.asio_watchdog_at = None;
                            // Decoder exited (EndStream): clear the dead sender so a
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
                    AsioEvent::Duration(d) => {
                        self.current_duration = d;
                        (self.callback)(PlayerEvent::Duration(d, self.current_seq));
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
                    AsioEvent::FormatUnsupported => {
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
                    }
                    AsioEvent::RateUnsupported => {
                        crate::vprintln!(
                            "[ASIO] device can't clock this track's rate; this track plays shared (ASIO stays on for other rates)"
                        );
                        if let Some(cancel) = self.asio_stream_cancel.take() {
                            cancel.store(true, Relaxed);
                        }
                        // Per-track skip: remember this track so its shared re-arm does NOT
                        // re-engage ASIO (loop), but keep ASIO on globally (no sticky clear --
                        // `AsioRateUnsupported` is not in the frontend's disable list).
                        self.asio_skip_track = self.current_track_id.clone();
                        self.is_asio_mode = false;
                        (self.callback)(PlayerEvent::DeviceError(
                            DeviceErrorKind::AsioRateUnsupported,
                        ));
                        self.rearm_shared_after_asio_failure();
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

        // Progress watchdog (a backstop): the clock reported Active but the position hasn't
        // advanced within the timeout -> the driver can't clock this track. Route it to shared
        // as a PER-TRACK skip (RateUnsupported), NOT a hard failure -- a single un-clockable
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
            // Mirror the RateUnsupported exit: remember the track so its shared re-arm doesn't
            // loop back into ASIO, and emit AsioRateUnsupported (not in the frontend's disable
            // list) so ASIO stays enabled.
            self.asio_skip_track = self.current_track_id.clone();
            self.is_asio_mode = false;
            self.asio_watchdog_at = None;
            (self.callback)(PlayerEvent::DeviceError(
                DeviceErrorKind::AsioRateUnsupported,
            ));
            self.rearm_shared_after_asio_failure();
        }

        // Debounced idle release (sustained pause or terminal stop): free the
        // ASIO driver so other apps regain the device. loading_gen blocks a slow
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
            // control thread dying, so it would leak (thread + full-track buffer).
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
        }
        if self.asio_teardown.is_none()
            && let Some((id, mode)) = self.pending_device_switch.take()
        {
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

    /// ASIO failed: re-arm a fresh shared load at the live position so the current track
    /// recovers immediately (the custom `deviceasio*` events aren't TIDAL-native, so the
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
            // Non-replayable source (DASH): no track to re-arm, so playback stops (as in the
            // device.rs switch paths). Logged so the silent stop is diagnosable.
            crate::vprintln!(
                "[ASIO] fallback: non-replayable source, playback stopped (cannot re-arm shared)"
            );
        }
    }

    /// Exclusive WASAPI failed (OS denied exclusive, or the device is locked): re-arm a
    /// fresh shared load at the live position so the current track keeps playing instead
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
        // the decode thread reads the RamBuffer in shared, exclusive, and ASIO alike, so the
        // governor's read_pos must track it regardless of `should_poll`. Gating it off froze
        // read_pos, so the governor saw an ever-growing `ahead`, paused the download, and never
        // resumed -- starving the decoder into silence.
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
        // below, so a seek landing this tick still counts as in flight: a range restart
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
                                // the multi-MB file unlocked so a concurrent
                                // track-load lookup isn't blocked behind it.
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
                    DecodeEvent::SeekComplete => {
                        // Only a decoder parked at EOF leaves a completion pending here, since
                        // Finished arms it once nothing is left to decode. The mute freezes the
                        // played count the drain check reads, which would take this for the end.
                        self.pending_complete = false;
                        self.played_samples
                            .store(self.decoded_samples.load(Relaxed), Relaxed);
                        if let Some(ref m) = self.cpal_muted {
                            m.store(false, Relaxed);
                        }
                        if self.seeking {
                            self.seeking = false;
                            self.seek_target = None;
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
                            // handle_seek announces Seeking over the state it interrupted; that
                            // state goes back here. is_playing covers every backend,
                            // stopped_retained the distinction it cannot make (stop versus pause).
                            (self.callback)(PlayerEvent::StateChange(
                                if self.is_playing {
                                    PlaybackState::Active
                                } else if self.stopped_retained {
                                    PlaybackState::Stopped
                                } else {
                                    PlaybackState::Paused
                                },
                                self.current_seq,
                            ));
                        }
                    }
                    DecodeEvent::Error(e) => {
                        crate::vprintln!("[DECODE] Error: {e}");
                        // Cached bytes that will not decode must not stay indexed: the load
                        // path already refreshed their LRU position, so nothing else would
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
                        // so nothing else clears committed_track. Drop it here so the SDK's
                        // same-track recovery reload rebuilds instead of resuming a dead decoder.
                        self.set_committed_track(None);
                        // Every site that reports an error returns right after, so the channel
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
                // early, so a pause there would commit both against an unfinished track.
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
