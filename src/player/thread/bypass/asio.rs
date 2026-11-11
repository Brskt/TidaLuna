use super::{seek_ack_is_current, verdict_names_current_stream};
use crate::player::asio::host::{AsioEvent, AsioHandle};
use crate::player::packet_failure::PacketFailure;
use crate::player::thread::PlayerThread;
use crate::player::thread::playback::stream_state_seq;
use crate::player::{ASIO_STREAM_SEQ, DeviceErrorKind, PlaybackState, PlayerEvent};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, mpsc};
use std::thread;

impl<F: Fn(PlayerEvent) + Send + 'static> PlayerThread<F> {
    /// Spawn the ASIO decoder for `buffer`, seeking the source to `seek_to`. Mirrors
    /// `spawn_exclusive_decoder`: a fresh `stream_id` makes the control thread drop
    /// the prior decoder's stale `PushPcm`; the new `StartStream` is the reset.
    #[cfg(target_os = "windows")]
    pub(in crate::player::thread) fn spawn_asio_decoder(
        &mut self,
        buffer: crate::player::buffer::RamBuffer,
        seek_to: Option<f64>,
        start_paused: bool,
    ) {
        let Some(cmd_tx) = self.asio_handle.as_ref().map(|h| h.command_sender()) else {
            return;
        };
        // Wake the retired reader to quiesce instead of parking on the read
        // timeout while the new stream contends for the same buffer.
        super::super::retire_reader(&mut self.asio_stream_cancel, self.current_buffer.as_ref());
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
        let opened_under = self.load_seq;
        thread::spawn(move || {
            if let Err(e) = crate::player::asio::host::stream_reader_to_asio(
                reader,
                stream_id,
                opened_under,
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
    pub(in crate::player::thread) fn start_asio_playback(
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
            let Some(handle) =
                AsioHandle::spawn(self.exclusive_gain.clone(), self.current_device_id.clone())
            else {
                // Same fallback as the two guards above: with no control thread there is no
                // ASIO pipeline, and the shared one is what actually gets played and polled.
                self.is_asio_mode = false;
                return false;
            };
            self.asio_handle = Some(handle);
        }
        // A queued user seek wins over the load-time resume position.
        let seek_to = self.take_start_position();
        self.spawn_asio_decoder(buffer, seek_to, start_paused);
        true
    }

    #[cfg(target_os = "windows")]
    pub(in crate::player::thread) fn poll_asio_events(&mut self) {
        if !self.is_asio_mode {
            return;
        }
        // Settled after the loop, outside the handle borrow: the settle takes `&mut self`,
        // where the arms below only touch fields.
        let mut decode_failure: Option<(String, PacketFailure)> = None;
        // Read before the drain and acted on after it, for the same borrow reason.
        let backend_died = self.asio_handle.as_ref().is_some_and(|h| h.is_dead());
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
                    AsioEvent::DecodeFailed {
                        stream_id,
                        error,
                        cause,
                    } => {
                        if self.current_asio_stream_id == Some(stream_id) {
                            crate::vprintln!("[ASIO] decoder died: {error}");
                            decode_failure = Some((error, cause));
                        }
                    }
                    // The id is the only scope this one gets: the control thread could not supply
                    // one, never having adopted the stream. The cause travels with it, classified
                    // at the probe: that read goes through the streaming buffer, which is the
                    // network path; a stall at track start is the same `TimedOut` a stall
                    // mid-track raises.
                    AsioEvent::StartupFailed {
                        stream_id,
                        error,
                        cause,
                    } => {
                        if self.current_asio_stream_id == Some(stream_id) {
                            crate::vprintln!("[ASIO] decoder never started: {error}");
                            decode_failure = Some((error, cause));
                        }
                    }
                    AsioEvent::StateChange {
                        opened_under,
                        state,
                    } => {
                        // Arm the progress watchdog when the clock starts; disarm on pause/stop.
                        if matches!(state, PlaybackState::Active) {
                            self.asio_watchdog_pos = self.last_asio_pos.unwrap_or(0.0);
                            self.asio_watchdog_at = Some(std::time::Instant::now());
                        } else if matches!(state, PlaybackState::Paused | PlaybackState::Stopped) {
                            self.asio_watchdog_at = None;
                        }
                        // Relayed whichever stream it describes, as the exclusive twin does.
                        (self.callback)(PlayerEvent::StateChange(
                            state,
                            stream_state_seq(opened_under, self.load_seq, self.current_seq),
                        ));
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
                            self.governor.buffer_progress().set_playback_active(false);
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
                            (self.callback)(PlayerEvent::Duration(
                                secs,
                                self.current_seq,
                                self.current_product_id.clone(),
                            ));
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
                        // refusal reads the TRACK's channels; a superseded verdict would
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
                        // Scoped whole, like `Completed` and `DecodeFailed` above, and unlike
                        // the exclusive twin, whose refusal takes the render thread down
                        // whichever stream it judged. No ASIO refusal does that, and acting on
                        // a superseded verdict would cancel a live decoder and demote a track
                        // the driver never refused.
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

        if let Some((error, cause)) = decode_failure {
            self.settle_bypass_decode_failure(error, cause);
        }

        // The control thread stopped without saying so: reaching here means a panic, every
        // typed exit reporting before returning. `is_finished` is a FACT, not a deadline, and
        // covers the two windows the watchdog cannot see (a death before `Active`, and one
        // while paused). The real-time callback stays out of reach: `extern "system"` aborts.
        if backend_died && self.current_asio_stream_id.is_some() {
            crate::verr!("[ASIO] control thread died; re-arming shared");
            // The decoder outlives it and would park forever against `consumed`, which
            // only the thread that just died advances.
            super::super::retire_reader(&mut self.asio_stream_cancel, self.current_buffer.as_ref());
            self.is_asio_mode = false;
            (self.callback)(PlayerEvent::DeviceError(DeviceErrorKind::AsioBackendDied));
            self.rearm_shared_after_asio_failure();
            self.asio_handle = None;
            return;
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
            super::super::retire_reader(&mut self.asio_stream_cancel, self.current_buffer.as_ref());
            self.asio_seek_tx = None;
            if let Some(handle) = self.asio_handle.take() {
                self.asio_teardown = handle.shutdown();
            }
            self.settle_seek_on_release();
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
    pub(in crate::player::thread) fn poll_asio_teardown(&mut self) {
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
    pub(super) fn rearm_shared_after_asio_failure(&mut self) {
        let was_playing = self.is_playing;
        self.has_track = false;
        self.is_playing = false;
        self.loading_gen = None;
        self.pending_play = None;
        self.seeking = false;
        self.seek_target = None;
        self.asio_seek_tx = None;

        // Prefer the live ASIO position (floor-free); fall back to resume_store.
        let retained = super::super::retained_track();
        let position = self.resume_position(self.last_asio_pos, retained.as_ref());
        self.last_asio_pos = None;
        if let Some(retained) = retained {
            (self.callback)(PlayerEvent::ReplayRequest {
                track: retained.track,
                expected_gen: retained.load_gen,
                position,
                play: was_playing,
            });
        } else {
            // Non-replayable source (DASH): no track to re-arm, and playback stops. Logged to
            // keep the silent stop diagnosable.
            crate::vprintln!(
                "[ASIO] fallback: non-replayable source, playback stopped (cannot re-arm shared)"
            );
        }
    }
}
