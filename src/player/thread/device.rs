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
            && requested_is_default == self.output_is_default
            && (self.current_output_name.as_deref() == Some(id.as_str())
                || super::output::resolved_device_name(&id).as_deref()
                    == self.current_output_name.as_deref())
        {
            // Persist on no-op too: the resolved compare can match a different
            // requested id, and loads/recovery resolve from current_device_id.
            self.current_device_id = Some(id);
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
                // re-assert returned above).
                let was_playing = self.is_playing;
                let position = self
                    .current_track_id
                    .as_deref()
                    .and_then(|tid| self.resume_store.get(tid))
                    .or_else(|| {
                        let p = self.current_position_secs();
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

                // ASIO bypasses the OS mixer, so seed the shared digital gain with the
                // effective volume (read BEFORE dropping volume_sync; same rationale
                // and saturation tradeoff as the exclusive seed below).
                let gain = self
                    .volume_sync
                    .as_ref()
                    .and_then(|vs| vs.get().ok())
                    .unwrap_or_else(|| f32::from_bits(self.last_volume.load(Relaxed)));
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
                self.asio_handle = Some(handle);
                self.current_device_id = Some(id.clone());

                // Re-arm with a FRESH load (the live buffer would race the decoder
                // probe); handle_load now takes the ASIO branch since is_asio_mode is set.
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
                // Position from resume_store, which is refreshed every ~200ms from
                // the real played time. current_position_secs() won't do: it drops
                // to near zero just after a switch/re-arm. Falls back to the live
                // position only when nothing is stored yet. Carried on the
                // ReplayRequest below, applied at the exclusive spawn.
                let was_playing = self.is_playing;
                let position = self
                    .current_track_id
                    .as_deref()
                    .and_then(|tid| self.resume_store.get(tid))
                    .or_else(|| {
                        let p = self.current_position_secs();
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

                // Exclusive bypasses the OS mixer, so seed the render's digital
                // gain with the effective volume (read BEFORE dropping volume_sync).
                // On a vs.get() failure use last_volume, not self.volume -- the
                // latter is pinned to 1.0 on the session-sync path and would seed
                // full scale (saturation). Breaks bit-perfect <100%, an accepted
                // tradeoff for a live slider.
                let gain = self
                    .volume_sync
                    .as_ref()
                    .and_then(|vs| vs.get().ok())
                    .unwrap_or_else(|| f32::from_bits(self.last_volume.load(Relaxed)));
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
                self.exclusive_handle = Some(handle);
                self.current_device_id = Some(id.clone());

                // Re-arm with a FRESH load, not the live buffer: symphonia's probe
                // races a mid-stream churned buffer and can hang, which means
                // silence. A fresh load probes cleanly; handle_load now takes the
                // exclusive branch since is_exclusive_mode is set.
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

                // The exclusive reader parks the buffer mid-range; re-probing here
                // would block the player loop. Re-arm a fresh shared load at the
                // live position instead (current_position_secs() is stale in
                // exclusive mode).
                let was_playing = self.is_playing;
                self.has_track = false;
                self.is_playing = false;
                self.loading_gen = None;
                self.pending_play = None;
                // A seek killed mid-flight by the exclusive entry (its SeekComplete
                // never arrives) would otherwise survive into the fresh pipeline and
                // pin the poll loop at 1ms reporting a ghost position.
                self.seeking = false;
                self.seek_target = None;

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

                let was_playing = self.is_playing;
                self.has_track = false;
                self.is_playing = false;
                self.loading_gen = None;
                self.pending_play = None;
                self.seeking = false;
                self.seek_target = None;

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
