use super::output::{STREAM_ERR_DEVICE_LOST, STREAM_ERR_NONE, STREAM_ERR_UNKNOWN};
use super::{DecodeEvent, PlayerThread};
use crate::player::{DeviceErrorKind, PlaybackState, PlayerEvent, format_ms};
use std::sync::atomic::Ordering::Relaxed;

#[cfg(target_os = "windows")]
use crate::player::asio::host::AsioEvent;
#[cfg(target_os = "windows")]
use crate::player::wasapi::ExclusiveEvent;

impl<F: Fn(PlayerEvent) + Send + 'static> PlayerThread<F> {
    pub(super) fn samples_to_secs(&self, samples: u64) -> f64 {
        let channels = self.channels.max(1) as u64;
        let frames = samples / channels;
        frames as f64 / self.sample_rate.max(1) as f64
    }

    pub(super) fn current_position_secs(&self) -> f64 {
        self.samples_to_secs(self.decoded_samples.load(Relaxed))
    }

    pub(super) fn played_position_secs(&self) -> f64 {
        self.samples_to_secs(self.played_samples.load(Relaxed))
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
                            eprintln!("[WASAPI] Init failed, falling back to shared mode: {e}");
                            if let Some(cancel) = self.exclusive_stream_cancel.take() {
                                cancel.store(true, Relaxed);
                            }
                            self.is_exclusive_mode = false;
                            (self.callback)(PlayerEvent::DeviceError(
                                DeviceErrorKind::ExclusiveModeNotAllowed,
                            ));
                        }
                        ExclusiveEvent::DeviceLocked(e) => {
                            eprintln!("[WASAPI] Device locked by another process: {e}");
                            if let Some(cancel) = self.exclusive_stream_cancel.take() {
                                cancel.store(true, Relaxed);
                            }
                            self.is_exclusive_mode = false;
                            (self.callback)(PlayerEvent::DeviceError(DeviceErrorKind::Locked));
                        }
                        ExclusiveEvent::Stopped => {
                            self.has_track = false;
                            self.is_playing = false;
                            self.current_track_id = None;
                            self.last_exclusive_pos = None;
                            crate::state::GOVERNOR
                                .buffer_progress()
                                .set_playback_active(false);
                            self.resume_store.flush_if_due(true);
                        }
                    }
                }
            }

            if !self.is_exclusive_mode {
                self.exclusive_handle = None;
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
                        (self.callback)(PlayerEvent::TimeUpdate(report, self.current_seq));
                    }
                    AsioEvent::StateChange(s) => {
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
                            self.last_asio_pos = None;
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
                    AsioEvent::InitFailed(e) => {
                        crate::vprintln!("[ASIO] Init failed, falling back to shared mode: {e}");
                        if let Some(cancel) = self.asio_stream_cancel.take() {
                            cancel.store(true, Relaxed);
                        }
                        self.is_asio_mode = false;
                        (self.callback)(PlayerEvent::DeviceError(DeviceErrorKind::AsioInitFailed));
                        self.rearm_shared_after_asio_failure();
                    }
                    AsioEvent::Stopped(sid) => {
                        // Ignore a Stopped from a superseded stream: the outgoing track's
                        // stop must not null a newer track's has_track (which forced a
                        // spurious re-arm/double-load when play arrived a loop-tick later).
                        if self.current_asio_stream_id == Some(sid) {
                            self.has_track = false;
                            self.is_playing = false;
                            self.current_track_id = None;
                            self.last_asio_pos = None;
                            crate::state::GOVERNOR
                                .buffer_progress()
                                .set_playback_active(false);
                            self.resume_store.flush_if_due(true);
                        }
                    }
                }
            }
        }

        if !self.is_asio_mode {
            self.asio_handle = None;
        }
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

        if !should_poll || !self.has_track || !self.is_playing {
            return;
        }

        // Detect decode thread stalling on RamBuffer (buffering)
        if !self.is_cached
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
                        if let Some(track_id) = self.current_track_id.as_ref() {
                            self.resume_store.clear(track_id);
                            self.resume_store.flush_if_due(true);
                        }

                        // Write to disk cache if complete and not already cached
                        if !self.is_cached
                            && let Some(ref buf) = self.current_buffer
                            && buf.is_complete()
                            && let Some(data) = buf.take_data()
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
                                    crate::player::cache::AudioCache::write_file(&path, &data)
                                {
                                    crate::vprintln!("[CACHE]  Store failed (write): {e}");
                                    return;
                                }
                                // Index insert + eviction under a short lock,
                                // skipped (and the file removed) if a cache clear
                                // raced this unlocked write.
                                let Ok(mut cache) = crate::state::AUDIO_CACHE.lock() else {
                                    crate::vprintln!("[CACHE]  Lock poisoned, skipping index");
                                    return;
                                };
                                match cache.record_if_current(
                                    &tid,
                                    &cache_format,
                                    data.len() as u64,
                                    store_gen,
                                ) {
                                    Ok(true) => crate::vprintln!(
                                        "[CACHE]  Stored: {} ({})",
                                        tid,
                                        crate::player::format_bytes(data.len() as u64)
                                    ),
                                    Ok(false) => crate::vprintln!(
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
                            (self.callback)(PlayerEvent::StateChange(
                                PlaybackState::Active,
                                self.current_seq,
                            ));
                        }
                    }
                    DecodeEvent::Error(e) => {
                        crate::vprintln!("[DECODE] Error: {e}");
                        (self.callback)(PlayerEvent::MediaError {
                            error: e,
                            code: "unreadable_file",
                        });
                    }
                }
            }
        }

        // Check if the ring buffer has drained after decode finished
        if self.pending_complete {
            let played = self.played_samples.load(Relaxed);
            if played > 0 && played == self.last_played_snapshot {
                crate::vprintln!(
                    "[DRAIN]  Ring buffer drained (played={}, decoded={})",
                    played,
                    self.decoded_samples.load(Relaxed),
                );
                self.pending_complete = false;
                self.last_played_snapshot = 0;
                (self.callback)(PlayerEvent::TimeUpdate(
                    self.current_duration,
                    self.current_seq,
                ));
                (self.callback)(PlayerEvent::StateChange(
                    PlaybackState::Completed,
                    self.current_seq,
                ));
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
            (self.callback)(PlayerEvent::TimeUpdate(target, self.current_seq));
        } else {
            let pos_secs = self.played_position_secs();
            let pos_secs = if self.current_duration > 0.0 {
                pos_secs.min(self.current_duration)
            } else {
                pos_secs
            };
            if pos_secs > 0.0 {
                if let Some(track_id) = self.current_track_id.as_ref() {
                    self.resume_store.set(track_id, pos_secs);
                    self.resume_store.flush_if_due(false);
                }
                (self.callback)(PlayerEvent::TimeUpdate(pos_secs, self.current_seq));
            }
        }
    }
}
