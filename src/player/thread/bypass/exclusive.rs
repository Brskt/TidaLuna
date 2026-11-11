use super::{seek_ack_is_current, verdict_names_current_stream};
use crate::player::packet_failure::PacketFailure;
use crate::player::thread::PlayerThread;
use crate::player::thread::playback::stream_state_seq;
use crate::player::wasapi::{ExclusiveCommand, ExclusiveEvent};
use crate::player::{DeviceErrorKind, EXCLUSIVE_STREAM_SEQ, PlaybackState, PlayerEvent, wasapi};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, mpsc};
use std::thread;

impl<F: Fn(PlayerEvent) + Send + 'static> PlayerThread<F> {
    /// Spawn the exclusive decoder for `buffer`, seeking the source to `seek_to`.
    /// A fresh `stream_id` makes the render drop the prior decoder's stale
    /// PushPcm. No `Stop` (it emits `Stopped`, which clears the host-side track);
    /// the new `StartStream` is the reset.
    #[cfg(target_os = "windows")]
    pub(in crate::player::thread) fn spawn_exclusive_decoder(
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
            // Wake the retired reader to see the cancel and quiesce instead
            // of parking up to the read timeout while the new stream contends for
            // the same buffer (matches handle_set_audio_device).
            if let Some(ref buf) = self.current_buffer {
                buf.wake_readers();
            }
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.exclusive_stream_cancel = Some(cancel.clone());

        let (seek_tx, seek_rx) = mpsc::channel::<(f64, u32)>();
        self.exclusive_seek_tx = Some(seek_tx);
        // Minted here, beside the stream_id, rather than at any call site: one caller
        // already forgot the sibling `seeking` reset, and the callee cannot be skipped.
        let seek_gen_id = self.next_seek_gen();
        // Per-stream consumed counter for the decoder's sent-minus-consumed throttle.
        let consumed = Arc::new(AtomicU64::new(0));
        // Fresh stream: don't inherit a stale reverse-to-shared position from a prior
        // exclusive session (the buffer-reuse mode switch bypasses handle_load's clear).
        self.last_exclusive_pos = None;

        let reader = buffer.clone().with_reader_cancel(cancel.clone());
        let total_len = buffer.total_len();
        let stream_id = EXCLUSIVE_STREAM_SEQ.fetch_add(1, Relaxed) + 1;
        // Record the live stream_id to keep Play/Pause stream-scoped: a premature
        // or stale command must not act on a superseded render context (mirrors
        // current_asio_stream_id).
        self.current_exclusive_stream_id = Some(stream_id);
        let opened_under = self.load_seq;
        thread::spawn(move || {
            if let Err(e) = wasapi::stream_flac_reader_to_wasapi(
                reader,
                total_len,
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
                crate::vprintln!("[WASAPI] Stream decode failed: {e}");
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
    pub(in crate::player::thread) fn start_exclusive_playback(
        &mut self,
        buffer: crate::player::buffer::RamBuffer,
        start_paused: bool,
    ) -> bool {
        if !self.is_exclusive_mode {
            return false;
        }
        // The new StartStream re-bases the render in place (no teardown that would
        // null the just-loaded track on a playlist-advance). A queued user seek wins
        // over the load-time resume position.
        let seek_to = self.take_start_position();
        self.spawn_exclusive_decoder(buffer, seek_to, start_paused);
        true
    }

    #[cfg(target_os = "windows")]
    pub(in crate::player::thread) fn poll_exclusive_events(&mut self) {
        if self.is_exclusive_mode {
            // Settled after the loop, outside the handle borrow: the settle takes `&mut
            // self`, where the arms below only touch fields.
            let mut decode_failure: Option<(String, PacketFailure)> = None;
            // Read before the drain and acted on after it, for the same borrow reason.
            let backend_died = self.exclusive_handle.as_ref().is_some_and(|h| h.is_dead());
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
                        ExclusiveEvent::DecodeFailed {
                            stream_id,
                            error,
                            cause,
                        } => {
                            if self.current_exclusive_stream_id == Some(stream_id) {
                                crate::vprintln!("[WASAPI] decoder died: {error}");
                                decode_failure = Some((error, cause));
                            }
                        }
                        // The id is the only scope this one gets: the render could not supply
                        // one, never having adopted the stream. The cause travels with it,
                        // classified at the probe: that read goes through the streaming buffer,
                        // which is the network path; a stall at track start is the same
                        // `TimedOut` a stall mid-track raises.
                        ExclusiveEvent::StartupFailed {
                            stream_id,
                            error,
                            cause,
                        } => {
                            if self.current_exclusive_stream_id == Some(stream_id) {
                                crate::vprintln!("[WASAPI] decoder never started: {error}");
                                decode_failure = Some((error, cause));
                            }
                        }
                        ExclusiveEvent::StateChange {
                            opened_under,
                            state,
                        } => {
                            // Relayed whichever stream it describes: the retired stream's
                            // pause is the page's only NOT_PLAYING in this mode. Only the seq
                            // names the stream; the load that replaced it never hears it.
                            (self.callback)(PlayerEvent::StateChange(
                                state,
                                stream_state_seq(opened_under, self.load_seq, self.current_seq),
                            ));
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
                                self.governor.buffer_progress().set_playback_active(false);
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
                            // Forwarded only for the stream still owned, which is what makes
                            // the id below the right one: it names the track this thread is
                            // committed to, and a superseded stream measured another.
                            if verdict_names_current_stream(
                                self.current_exclusive_stream_id,
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

            if let Some((error, cause)) = decode_failure {
                self.settle_bypass_decode_failure(error, cause);
            }

            // The render thread stopped without saying so. Reaching here means a panic,
            // because every typed exit reports before returning: no error value, and no
            // hook turns one into anything. Judged on `is_finished`, a FACT not a deadline:
            // there is no healthy state it can misread, which is why polling it is safe
            // where a timer would not be.
            if backend_died && self.current_exclusive_stream_id.is_some() {
                crate::verr!("[WASAPI] render thread died; re-arming shared");
                // The decoder outlives it and would park forever: it throttles against
                // `consumed`, which only the thread that just died advances. Cancelling
                // first is what the idle release already owes and documents; skipping it
                // leaks the thread AND its whole-track buffer.
                if let Some(cancel) = self.exclusive_stream_cancel.take() {
                    cancel.store(true, Relaxed);
                    if let Some(ref buf) = self.current_buffer {
                        buf.wake_readers();
                    }
                }
                self.is_exclusive_mode = false;
                (self.callback)(PlayerEvent::DeviceError(
                    DeviceErrorKind::ExclusiveBackendDied,
                ));
                self.rearm_shared_after_exclusive_failure();
            }

            if !self.is_exclusive_mode {
                self.exclusive_handle = None;
            }

            // Debounced release: once a pause has lingered past EXCLUSIVE_PAUSE_RELEASE
            // without a resume or track change, hand the exclusive device back, letting
            // other apps regain it. has_track=false + committed cleared makes the next play
            // re-arm (decide_play ReArm) or a same-track reload reopen (no idempotent
            // skip); both reopen the client. `loading_gen` blocks an in-flight load, LoadStarted
            // landing well before handle_load clears the timer. Re-read each tick rather than
            // cleared at that transition, a hand-cleared deadline being the omission that opened
            // this window; `LoadSettleGuard` drops on every load exit, so the term cannot stick.
            if self.is_exclusive_mode
                && !self.is_playing
                && self.loading_gen.is_none()
                && let Some(at) = self.exclusive_release_at
                && std::time::Instant::now() >= at
            {
                self.exclusive_release_at = None;
                if let Some(ref handle) = self.exclusive_handle {
                    handle.send(ExclusiveCommand::ReleaseDevice);
                }
                self.settle_seek_on_release();
                self.has_track = false;
                self.set_committed_track(None);
                crate::vprintln!(
                    "[WASAPI] Released exclusive device on stop (other apps can use it)"
                );
            }
        }
    }

    /// Exclusive WASAPI failed (OS denied exclusive, or the device is locked): re-arm a
    /// fresh shared load at the live position; the current track keeps playing instead
    /// of stopping. Mirrors `rearm_shared_after_asio_failure`. No-op without a track.
    #[cfg(target_os = "windows")]
    pub(super) fn rearm_shared_after_exclusive_failure(&mut self) {
        let was_playing = self.is_playing;
        self.has_track = false;
        self.is_playing = false;
        self.loading_gen = None;
        self.pending_play = None;
        self.seeking = false;
        self.seek_target = None;
        self.exclusive_seek_tx = None;
        // Left standing, the panic gate fires a second recovery on the tick a typed refusal
        // already ran: `is_dead()` reports a clean exit exactly as it reports a panic. Safe to
        // clear on this backend only, every exclusive refusal taking the render thread down.
        // The ASIO twin keeps its id: no ASIO refusal ends its control thread.
        self.current_exclusive_stream_id = None;

        // Prefer the live exclusive position (floor-free); fall back to resume_store.
        let retained = crate::state::CURRENT_TRACK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retained()
            .cloned();
        let position = self.last_exclusive_pos.or_else(|| {
            retained.as_ref().and_then(|r| {
                self.resume_store
                    .get(&crate::player::canonical_track_id(&r.track.url))
            })
        });
        self.last_exclusive_pos = None;
        if let Some(retained) = retained {
            (self.callback)(PlayerEvent::ReplayRequest {
                track: retained.track,
                expected_gen: retained.load_gen,
                position,
                play: was_playing,
            });
        } else {
            crate::vprintln!(
                "[WASAPI] exclusive fallback: non-replayable source, playback stopped (cannot re-arm shared)"
            );
        }
    }
}
