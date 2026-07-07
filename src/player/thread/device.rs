use super::decode::{DecodeThreadConfig, spawn_decode_thread};
use super::output::open_output_stream;
use super::{DecodeCommand, PlayerThread};
use crate::player::buffer::RamBuffer;
use crate::player::{DeviceErrorKind, OutputMode, PlayerEvent};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;

#[cfg(target_os = "windows")]
use crate::player::asio::host::AsioHandle;
#[cfg(target_os = "windows")]
use crate::player::wasapi::ExclusiveHandle;

impl<F: Fn(PlayerEvent) + Send + 'static> PlayerThread<F> {
    pub(super) fn handle_set_audio_device(&mut self, id: String, mode: OutputMode) {
        // TIDAL re-emits player.devices.set (same device) right before play; rebuilding
        // here stops the decoder and reopens cpal, so the unpause reloads and reseeks.
        // No-op only while the shared stream is live (a dead one must still retry).
        #[cfg(target_os = "windows")]
        let shared_reassert =
            mode == OutputMode::Shared && !self.is_asio_mode && !self.is_exclusive_mode;
        #[cfg(not(target_os = "windows"))]
        let shared_reassert = mode == OutputMode::Shared;
        // No-op only if the request resolves to the open device and keeps the same
        // follow-class (see output_is_default); a class flip on one device must
        // rebuild. Fast-path id == the open name to skip the resolve.
        let requested_is_default = super::output::is_default_selector(&id);
        if shared_reassert
            && self.has_track
            && self.cpal_stream.is_some()
            && self.current_output_name.is_some()
            && (self.current_output_name.as_deref() == Some(id.as_str())
                || super::output::resolved_device_name(&id).as_deref()
                    == self.current_output_name.as_deref())
        {
            // Same physical device -- no rebuild, even across an id-class flip. Our own
            // ASIO-off toggle sends "auto" while TIDAL re-asserts the concrete device id;
            // gating on `requested_is_default == output_is_default` would force a rebuild
            // that raced effective_position() to a stale zero and restarted the track. Adopt the new
            // id AND its follow-class so a later default-device change is still handled.
            self.current_device_id = Some(id);
            self.output_is_default = requested_is_default;
            return;
        }
        #[cfg(target_os = "windows")]
        {
            // Idempotent ASIO re-assert: TIDAL re-emits player.devices.set around play/track
            // changes. Already in ASIO mode, this must be a pure no-op -- the decoder-cancel
            // below would kill the live decoder with no respawn, leaving the ring silent.
            if mode == OutputMode::Asio && self.is_asio_mode {
                self.current_device_id = Some(id);
                return;
            }
            // Exclusive omits has_track (unlike shared): a track-change set arrives
            // after stop cleared it and re-arming would double-load. is_exclusive_mode
            // implies a live handle, so a dead pipeline still re-arms.
            if mode == OutputMode::Exclusive
                && self.is_exclusive_mode
                && self.current_device_id.as_deref() == Some(id.as_str())
            {
                return;
            }
            // Non-replayable source (DASH nulls CURRENT_TRACK): the bypass switches below can't
            // re-arm it. Bail BEFORE the decoder-cancel (cancelling a live bypass decoder then
            // returning would strand the mode with no producer); gated on a live bypass decoder.
            if self.has_track
                && (self.is_asio_mode || self.is_exclusive_mode)
                && crate::state::CURRENT_TRACK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none()
            {
                crate::vprintln!("[AUDIO] Device switch skipped: non-replayable source active");
                return;
            }
            // Per-track mode skip: bail BEFORE retiring the live reader below. Toggling to
            // a mode this track can't do (e.g. ASIO while it plays in exclusive due to
            // asio_skip_track) must not kill the live decoder with nothing re-armed -- that
            // leaves silence with stale is_playing/mode flags. Keep the current backend
            // playing; the skip clears on the next different-track load.
            if mode == OutputMode::Asio
                && self.current_track_id.is_some()
                && self.asio_skip_track.as_deref() == self.current_track_id.as_deref()
            {
                self.current_device_id = Some(id);
                crate::vprintln!(
                    "[ASIO] skip: device can't clock this track's rate; keeping the current output"
                );
                return;
            }
            if mode == OutputMode::Exclusive
                && self.current_track_id.is_some()
                && self.exclusive_skip_track.as_deref() == self.current_track_id.as_deref()
            {
                self.current_device_id = Some(id);
                crate::vprintln!(
                    "[WASAPI] skip: device can't do this track's format in exclusive; keeping the current output"
                );
                return;
            }
            // Retire any live exclusive/asio reader before the new stream contends
            // for the same buffer.
            if let Some(cancel) = self.exclusive_stream_cancel.take() {
                cancel.store(true, Relaxed);
                if let Some(ref buf) = self.current_buffer {
                    buf.wake_readers();
                }
            }
            if let Some(cancel) = self.asio_stream_cancel.take() {
                cancel.store(true, Relaxed);
                if let Some(ref buf) = self.current_buffer {
                    buf.wake_readers();
                }
            }

            // Switch TO ASIO (radio: mutually exclusive with shared / WASAPI-exclusive).
            if mode == OutputMode::Asio {
                // Reaching here is a real shared/exclusive -> ASIO switch (the idempotent
                // re-assert and the per-track skip both returned above, pre-cancel).
                let was_playing = self.is_playing;
                let position = self
                    .current_track_id
                    .as_deref()
                    .and_then(|tid| self.resume_store.get(tid))
                    .or_else(|| {
                        let p = self.played_position_secs();
                        (p > 0.0).then_some(p)
                    });

                // Non-replayable source (DASH clears CURRENT_TRACK): no-op the switch
                // instead of killing playback (the re-arm below needs a track).
                let replayable = crate::state::CURRENT_TRACK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some();
                if self.has_track && !replayable {
                    crate::vprintln!("[AUDIO] ASIO switch skipped: non-replayable source active");
                    return;
                }

                self.stop_decode();
                self.cpal_stream = None;
                self.cpal_stream_error = None;

                // ASIO bypasses the OS mixer, so seed the shared digital gain from the
                // app's own volume state (last_volume), NOT a live GetMasterVolume()
                // query: a fresh session reports 1.0 regardless of the real level (MS
                // docs), which would blast full scale. Matches JUCE/foobar/JRiver --
                // exclusive/ASIO gain comes from app state, never a session read.
                let gain = f32::from_bits(self.last_volume.load(Relaxed));
                self.exclusive_gain.store(f32::to_bits(gain), Relaxed);
                crate::vprintln!("[VOLUME] asio gain seeded: {gain:.3}");
                self.volume_sync = None;
                self.volume_rx = None;

                // Radio: tear down a live exclusive handle, then (re)spawn the ASIO one.
                if let Some(old) = self.exclusive_handle.take() {
                    old.shutdown();
                }
                self.is_exclusive_mode = false;
                if let Some(old) = self.asio_handle.take() {
                    old.shutdown();
                }
                let handle = AsioHandle::spawn(self.exclusive_gain.clone());
                self.is_asio_mode = true;
                // Drop any stale release timer from a prior ASIO session so a fresh
                // engagement can't fire it and shut down the handle we just spawned.
                self.asio_release_at = None;
                self.asio_handle = Some(handle);
                self.current_device_id = Some(id.clone());

                // Reuse the retained buffer when the track is fully in memory: spawn the
                // ASIO decoder on it directly, no network re-download and no idle stall.
                // Only a still-STREAMING buffer races the decoder probe (its base_offset
                // churns mid-download); a complete one is probe-safe (the same invariant
                // cache-hit loads rely on every day). No new load: clear the load bookkeeping.
                if let Some(buffer) = self.current_buffer.clone().filter(RamBuffer::is_reusable) {
                    self.loading_gen = None;
                    self.pending_play = None;
                    crate::vprintln!("[AUDIO] Switched to ASIO output (retained buffer)");
                    self.spawn_asio_decoder(buffer, position, !was_playing);
                    return;
                }
                // Still-streaming buffer: a fresh load re-probes cleanly (handle_load takes
                // the ASIO branch since is_asio_mode is set).
                self.has_track = false;
                self.is_playing = false;
                self.loading_gen = None;
                self.pending_play = None;
                self.seeking = false;
                self.seek_target = None;
                let track = crate::state::CURRENT_TRACK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                crate::vprintln!("[AUDIO] Switched to ASIO output");
                if let Some(track) = track {
                    (self.callback)(PlayerEvent::ReplayRequest {
                        track,
                        expected_gen: crate::player::LOAD_SEQ.load(Relaxed),
                        position,
                        play: was_playing,
                    });
                }
                return;
            }

            if mode == OutputMode::Exclusive {
                // (The per-track format skip returned above, before the cancel block.)
                // Position from resume_store (refreshed every ~200ms from the played
                // time); falls back to the live played position when nothing is stored
                // yet. Carried on the ReplayRequest below (or the retained-buffer reuse),
                // applied at the exclusive spawn.
                let was_playing = self.is_playing;
                let position = self
                    .current_track_id
                    .as_deref()
                    .and_then(|tid| self.resume_store.get(tid))
                    .or_else(|| {
                        let p = self.played_position_secs();
                        (p > 0.0).then_some(p)
                    });

                // Non-replayable source (DASH clears CURRENT_TRACK): the teardown
                // re-arms via ReplayRequest, which needs a track, so no-op the
                // switch instead of killing playback.
                let replayable = crate::state::CURRENT_TRACK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some();
                if self.has_track && !replayable {
                    crate::vprintln!(
                        "[AUDIO] Exclusive switch skipped: non-replayable source active"
                    );
                    return;
                }

                self.stop_decode();
                self.cpal_stream = None;
                // Drop the shared-stream error flag too: a stale device-loss signal must not
                // trigger a shared-cpal rebuild while we are in exclusive mode.
                self.cpal_stream_error = None;

                // Exclusive bypasses the OS mixer, so seed the render's digital gain
                // from the app's own volume state (last_volume), NOT a live
                // GetMasterVolume() query: a fresh session reports 1.0 regardless of
                // the real level (MS docs), which would blast full scale. Breaks
                // bit-perfect <100%, an accepted tradeoff for a live slider.
                let gain = f32::from_bits(self.last_volume.load(Relaxed));
                self.exclusive_gain.store(f32::to_bits(gain), Relaxed);
                crate::vprintln!("[VOLUME] exclusive gain seeded: {gain:.3}");

                self.volume_sync = None;
                self.volume_rx = None;

                // Radio: tear down a live ASIO handle first.
                if let Some(old) = self.asio_handle.take() {
                    old.shutdown();
                }
                self.is_asio_mode = false;
                if let Some(old) = self.exclusive_handle.take() {
                    old.shutdown();
                }
                let handle = ExclusiveHandle::spawn(id.clone(), self.exclusive_gain.clone());
                self.is_exclusive_mode = true;
                // Drop any stale release timer from a prior exclusive session so a fresh
                // engagement can't fire it and release the device we just acquired.
                self.exclusive_release_at = None;
                self.exclusive_handle = Some(handle);
                self.current_device_id = Some(id.clone());

                // Reuse the retained buffer when the track is fully in memory: spawn the
                // exclusive decoder on it directly, no network re-download and no idle stall.
                // The probe race that motivated the fresh load only affects a still-STREAMING
                // buffer (its base_offset churns mid-download); a complete one is probe-safe.
                if let Some(buffer) = self.current_buffer.clone().filter(RamBuffer::is_reusable) {
                    self.loading_gen = None;
                    self.pending_play = None;
                    self.seeking = false;
                    self.seek_target = None;
                    crate::vprintln!(
                        "[AUDIO] Switched to exclusive WASAPI (retained buffer): {}",
                        id
                    );
                    self.spawn_exclusive_decoder(buffer, position, !was_playing);
                    return;
                }
                // Still-streaming buffer: symphonia's probe would race the mid-stream churn
                // (silence), so re-arm a fresh load that probes cleanly; handle_load takes
                // the exclusive branch since is_exclusive_mode is set.
                self.has_track = false;
                self.is_playing = false;
                self.loading_gen = None;
                self.pending_play = None;
                self.seeking = false;
                self.seek_target = None;
                let track = crate::state::CURRENT_TRACK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                crate::vprintln!("[AUDIO] Switched to exclusive WASAPI: {}", id);
                if let Some(track) = track {
                    (self.callback)(PlayerEvent::ReplayRequest {
                        track,
                        expected_gen: crate::player::LOAD_SEQ.load(Relaxed),
                        position,
                        play: was_playing,
                    });
                }
                return;
            } else if self.is_exclusive_mode {
                // Same non-replayable guard: with no CURRENT_TRACK there's nothing
                // to re-arm, so no-op instead of killing playback.
                let replayable = crate::state::CURRENT_TRACK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some();
                if self.has_track && !replayable {
                    crate::vprintln!("[AUDIO] Shared switch skipped: non-replayable source active");
                    return;
                }

                if let Some(old) = self.exclusive_handle.take() {
                    old.shutdown();
                }
                self.is_exclusive_mode = false;
                self.current_device_id = Some(id);

                // Prefer the live exclusive position (floor-free); fall back to
                // resume_store only if no TimeUpdate arrived (it floors sub-1s and
                // can return a stale prior-session offset). Track from CURRENT_TRACK
                // since a polled Stopped may have nulled current_track_id.
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

                // Reuse the retained buffer when the track is fully in memory: rebuild the
                // shared pipeline on it in place (the device-switch path), no network
                // re-download and no idle stall. A seek killed mid-flight by the exclusive
                // entry must not survive into the rebuilt pipeline (it would pin the poll
                // loop at a ghost position), so clear it first.
                if self
                    .current_buffer
                    .as_ref()
                    .is_some_and(RamBuffer::is_reusable)
                {
                    self.seeking = false;
                    self.seek_target = None;
                    crate::vprintln!("[AUDIO] Switched back to shared mode (retained buffer)");
                    self.rebuild_pipeline_at(position.unwrap_or(0.0));
                    return;
                }

                // Still-streaming buffer: the exclusive reader parks it mid-range, so a
                // fresh shared load probes cleanly instead of blocking the player loop.
                let was_playing = self.is_playing;
                self.has_track = false;
                self.is_playing = false;
                self.loading_gen = None;
                self.pending_play = None;
                self.seeking = false;
                self.seek_target = None;
                crate::vprintln!("[AUDIO] Switched back to shared mode");
                if let Some(track) = track {
                    (self.callback)(PlayerEvent::ReplayRequest {
                        track,
                        expected_gen: crate::player::LOAD_SEQ.load(Relaxed),
                        position,
                        play: was_playing,
                    });
                }
                return;
            } else if self.is_asio_mode {
                // asio -> shared (same non-replayable guard as the exclusive path).
                let replayable = crate::state::CURRENT_TRACK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some();
                if self.has_track && !replayable {
                    crate::vprintln!("[AUDIO] Shared switch skipped: non-replayable source active");
                    return;
                }

                if let Some(old) = self.asio_handle.take() {
                    old.shutdown();
                }
                self.is_asio_mode = false;
                self.current_device_id = Some(id);

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

                // Reuse the retained buffer when the track is fully in memory: rebuild the
                // shared pipeline on it in place (the device-switch path), no network
                // re-download and no idle stall.
                if self
                    .current_buffer
                    .as_ref()
                    .is_some_and(RamBuffer::is_reusable)
                {
                    self.seeking = false;
                    self.seek_target = None;
                    crate::vprintln!(
                        "[AUDIO] Switched back to shared mode (from ASIO, retained buffer)"
                    );
                    self.rebuild_pipeline_at(position.unwrap_or(0.0));
                    return;
                }

                // Still-streaming buffer: a fresh shared load probes cleanly.
                let was_playing = self.is_playing;
                self.has_track = false;
                self.is_playing = false;
                self.loading_gen = None;
                self.pending_play = None;
                self.seeking = false;
                self.seek_target = None;
                crate::vprintln!("[AUDIO] Switched back to shared mode (from ASIO)");
                if let Some(track) = track {
                    (self.callback)(PlayerEvent::ReplayRequest {
                        track,
                        expected_gen: crate::player::LOAD_SEQ.load(Relaxed),
                        position,
                        play: was_playing,
                    });
                }
                return;
            }
        }

        self.current_device_id = Some(id.clone());
        self.rebuild_pipeline_at(self.effective_position());
        crate::vprintln!("[AUDIO] Switched to device: {}", id);
    }

    /// Tear down the cpal stream and rebuild the pipeline on the resolved device at
    /// `position`, preserving play state. No-op when no track is loaded. Shared by
    /// manual device switching and device-loss recovery.
    fn rebuild_pipeline_at(&mut self, position: f64) {
        if !self.has_track {
            return;
        }
        let was_playing = self.is_playing;
        self.stop_decode();
        self.cpal_stream = None;
        if let Some(ref buffer) = self.current_buffer {
            let buffer = buffer.clone();
            self.rebuild_pipeline_on_device(buffer, was_playing, position);
        }
    }

    /// Recovery path for device loss/invalidation (cpal `DeviceNotAvailable`/
    /// `StreamInvalidated`): rebuild on the current default device at the last-heard
    /// position. cpal only auto-reroutes on a default-device *change*, not on loss.
    pub(super) fn recover_audio_device(&mut self) {
        self.rebuild_pipeline_at(self.effective_position());
    }

    pub(super) fn rebuild_pipeline_on_device(
        &mut self,
        buffer: RamBuffer,
        was_playing: bool,
        seek_to: f64,
    ) {
        // Track unchanged on a switch/recovery, so the format is known. Re-probing
        // here would block the player thread on a mid-stream buffer, which means
        // silence since the pipeline is already torn down. Reuse the format for the
        // cpal open; the decode thread re-probes on its own thread.
        let sr = self.sample_rate;
        let ch = self.channels;

        let Some(device) = self.resolve_output_device() else {
            return;
        };

        let opened = match open_output_stream(&device, sr, ch, &self.volume) {
            Some(o) => o,
            None => {
                (self.callback)(PlayerEvent::DeviceError(
                    DeviceErrorKind::FormatNotSupported,
                ));
                return;
            }
        };

        let actual_rate = opened.rate;
        let actual_channels = opened.channels;
        self.cpal_muted = Some(opened.muted);
        self.cpal_mute_ack = Some(opened.mute_ack);
        self.cpal_stream_error = Some(opened.stream_error);
        self.played_samples = opened.played_samples;

        self.sample_rate = actual_rate;
        self.channels = actual_channels;
        self.decoded_samples.store(0, Relaxed);
        self.played_samples.store(0, Relaxed);

        let (decode_cmd_tx, decode_cmd_rx) = mpsc::channel();
        let (decode_event_tx, decode_event_rx) = mpsc::channel();
        let decoded_samples = self.decoded_samples.clone();

        let decode_buffer = buffer.clone();
        let decode_handle = spawn_decode_thread(DecodeThreadConfig {
            buffer: decode_buffer,
            producer: opened.producer,
            decoded_samples,
            cmd_rx: decode_cmd_rx,
            event_tx: decode_event_tx,
            output_rate: actual_rate,
            output_channels: actual_channels,
            seek_gen: opened.seek_gen,
        });

        let stream = opened.stream;

        self.cpal_stream = Some(stream);
        self.decode_cmd_tx = Some(decode_cmd_tx);
        self.decode_event_rx = Some(decode_event_rx);
        self.decode_handle = Some(decode_handle);
        self.current_buffer = Some(buffer);

        #[cfg(target_os = "windows")]
        self.init_volume_sync();

        if seek_to > 0.0
            && let Some(ref tx) = self.decode_cmd_tx
        {
            let _ = tx.send(DecodeCommand::Seek(seek_to));
        }

        if was_playing {
            self.start_playback();
        }
    }
}
