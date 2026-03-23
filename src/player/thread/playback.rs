use super::{DecodeEvent, PlayerThread};
use crate::player::{DeviceErrorKind, PlaybackState, PlayerEvent, format_ms};
use std::sync::atomic::Ordering::Relaxed;

#[cfg(target_os = "windows")]
use crate::player::wasapi::{ExclusiveCommand, ExclusiveEvent};

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

    pub(super) fn poll_playback(&mut self) {
        #[cfg(target_os = "windows")]
        let should_poll = !self.is_exclusive_mode;
        #[cfg(not(target_os = "windows"))]
        let should_poll = true;

        // Always update governor buffer progress when a track is loaded
        if should_poll
            && self.has_track
            && let Some(ref buf) = self.current_buffer
        {
            let bp = crate::state::GOVERNOR.buffer_progress();
            bp.written.store(buf.written(), Relaxed);
            bp.read_pos.store(buf.read_cursor(), Relaxed);
        }

        // Detect cpal stream errors
        if let Some(ref flag) = self.cpal_stream_error {
            let code = flag.swap(0, Relaxed);
            if code == 1 {
                (self.callback)(PlayerEvent::DeviceError(DeviceErrorKind::Disconnected));
            } else if code == 2 {
                (self.callback)(PlayerEvent::DeviceError(DeviceErrorKind::Unknown));
            }
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
                            std::thread::spawn(move || {
                                if let Some(tid) = track_id {
                                    let Ok(mut cache) = crate::state::AUDIO_CACHE.lock() else {
                                        eprintln!("[CACHE]  Lock poisoned, skipping store");
                                        return;
                                    };
                                    match cache.store(&tid, "flac", &data) {
                                        Ok(()) => {
                                            crate::vprintln!(
                                                "[CACHE]  Stored: {} ({})",
                                                tid,
                                                crate::player::format_bytes(data.len() as u64)
                                            );
                                        }
                                        Err(e) => {
                                            eprintln!("[CACHE]  Store failed: {e}");
                                        }
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
