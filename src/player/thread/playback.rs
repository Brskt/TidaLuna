use super::crossfade::IncomingFailure;
use super::output::{STREAM_ERR_DEVICE_LOST, STREAM_ERR_NONE, STREAM_ERR_UNKNOWN};
use super::{DecodeEvent, PlayerThread};
use crate::player::{DeviceErrorKind, MediaErrorCode, PlaybackState, PlayerEvent, format_ms};
use std::sync::atomic::Ordering::Relaxed;

/// Whether a fatal `DecodeEvent::Error` must be settled in place: the drain path
/// only takes over after a trailing `Finished` and with at least one sample
/// played - otherwise nothing else ever emits a terminal state. Pure, to
/// be unit-tested without the audio pipeline.
fn decode_failure_needs_settle(pending_complete: bool, played_samples: u64) -> bool {
    !pending_complete || played_samples == 0
}

/// The seq a transport state of a bypass stream travels under. A stream the current load
/// opened is answered under `current_seq`, which a stop since `load_seq` has moved on and
/// which the SDK and the Connect receiver both read as "this load". A stream an earlier load
/// opened keeps that load's seq: the backend reports on its own clock, a render stop and reset
/// can outlast the track change that retired the stream, and its state must still read as
/// before the load that replaced it. Pure, to be unit-tested without a backend.
#[cfg(any(target_os = "windows", test))]
pub(super) fn stream_state_seq(opened_under: u32, load_seq: u32, current_seq: u32) -> u32 {
    if opened_under < load_seq {
        opened_under
    } else {
        current_seq
    }
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

    /// Persist the ciphertext the download staged for whichever track is current.
    /// The decoded bytes stay in the buffer: taking them would empty it and cost the
    /// track its reusability (see `RamBuffer::is_reusable`).
    ///
    /// Reachable from both the ordinary end-of-decode path and a crossfade promotion whose
    /// incoming track finished during the overlap: such a track has its `Finished` consumed by
    /// the fade's own drain, and would otherwise never be cached.
    pub(super) fn store_finished_ciphertext(&self) {
        if self.cached.is_some() {
            return;
        }
        let Some(ref buf) = self.current_buffer else {
            return;
        };
        if !buf.is_complete() {
            return;
        }
        let Some((staged, len)) = buf.take_ciphertext() else {
            return;
        };
        let Some(track_id) = self.current_track_id.clone() else {
            return;
        };
        crate::player::cache::AudioCache::store_ciphertext_detached(
            track_id,
            self.current_format.clone(),
            staged,
            len,
        );
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
            let bp = self.governor.buffer_progress();
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
            && self.cached.is_none()
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
        // While a fade runs, `decode_event_rx` still belongs to the OUTGOING
        // decoder; its end-of-stream is the one the callback is waiting for
        // before it may retire that ring. An error counts too: a decoder that died
        // will produce nothing more, and without this the fade would never conclude.
        let mut outgoing_reached_end = false;
        if let Some(ref rx) = self.decode_event_rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    DecodeEvent::Finished => {
                        outgoing_reached_end = true;
                        crate::vprintln!(
                            "[TRACK]  Decode finished ({})",
                            if self.cached.is_some() {
                                "from cache"
                            } else {
                                "from stream"
                            }
                        );
                        self.store_finished_ciphertext();

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
                                if self.cached.is_some() {
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
                            // The answer carries its own position, as both bypass backends do.
                            // The periodic tick below cannot stand in for it: a pause returns
                            // early there, leaving the target handle_seek announced on screen
                            // until playback resumes.
                            (self.callback)(PlayerEvent::TimeUpdate(settled, self.current_seq));
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
                    DecodeEvent::NetworkStalled => {
                        outgoing_reached_end = true;
                        // No duration named: the deadline is whichever budget expires
                        // first, and that is the download's eight reconnects.
                        crate::verr!("[DECODE] Network lost, stopping the player");
                        // Deliberately none of what the error arm below does. `drop_entry`
                        // would evict a file that decoded fine until the bytes stopped,
                        // and every `MediaError` code TIDAL maps advances the queue.
                        (self.callback)(PlayerEvent::NetworkLost(self.current_seq));
                        // The decode thread is gone; leaving these set arms a guard that
                        // nothing can clear, exactly as in the error arm.
                        self.set_committed_track(None);
                        self.decode_cmd_tx = None;
                        // `pending_complete` stays unset; nothing else here would ever
                        // emit a terminal state. Without this settle `is_playing`,
                        // `has_track` and the cpal stream outlive a dead decoder, and the
                        // next Play reports Active over permanent silence.
                        fatal_decode = true;
                    }
                    DecodeEvent::Stopped => {
                        outgoing_reached_end = true;
                        crate::vprintln!("[DECODE] Stopped");
                        // No banner and no cache drop: nothing failed, and the bytes of a stop
                        // are good. The settle is still owed, because a stop arriving here
                        // came from a path that kept this receiver and nothing else will end
                        // the track: left unsettled, `is_playing`, `has_track` and the cpal
                        // stream outlive a dead decoder and the next Play reports Active over
                        // silence.
                        self.set_committed_track(None);
                        self.decode_cmd_tx = None;
                        fatal_decode = true;
                    }
                    DecodeEvent::Error(e) => {
                        outgoing_reached_end = true;
                        crate::vprintln!("[DECODE] Error: {e}");
                        // Cached bytes that will not decode must not stay indexed: the load
                        // path already refreshed their LRU position, so nothing else would
                        // retire them and every later play would fail the same way. Handed
                        // over rather than done here, this being the control thread where a
                        // cache wipe holds the lock for its whole duration.
                        if let Some(seen) = self.cached
                            && let Some(tid) = self.current_track_id.clone()
                        {
                            crate::player::cache::AudioCache::drop_entry_detached(tid, seen);
                        }
                        (self.callback)(PlayerEvent::MediaError {
                            error: e,
                            code: MediaErrorCode::UnreadableFile,
                            seq: self.current_seq,
                            product_id: None,
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

        if outgoing_reached_end && let Some(xfade) = self.cpal_xfade.as_ref() {
            xfade.out_eof.store(true, Relaxed);
        }

        // The INCOMING decoder has its own channel and nothing else reads it while the fade
        // runs. Left undrained, a decode failure there was invisible for the whole overlap and
        // surfaced at promotion as a full stop with no advance. Handled separately from the
        // outgoing decoder's events: they describe a different track.
        let mut incoming_failed: Option<IncomingFailure> = None;
        let mut incoming_finished = false;
        if let Some(ref pending) = self.pending_crossfade {
            while let Ok(event) = pending.event_rx.try_recv() {
                match event {
                    DecodeEvent::Error(e) => {
                        incoming_failed = Some(IncomingFailure::Decode(e));
                        break;
                    }
                    // The fade is best-effort: an incoming track whose bytes dried up
                    // costs the overlap, not the track still playing. No banner from
                    // here: the listener is hearing music, and nothing has failed for
                    // them yet.
                    DecodeEvent::NetworkStalled => {
                        incoming_failed = Some(IncomingFailure::NetworkStalled);
                        break;
                    }
                    // Shorter than the fade. Recorded rather than acted on: the swap
                    // still has to happen, and the completion it implies belongs
                    // after the promotion, not to the track still playing.
                    DecodeEvent::Finished => incoming_finished = true,
                    // Asked for by the very caller that is tearing this fade down, and not a
                    // failure. Recorded as one it would discard the staged bytes, which are
                    // still good, and cancel a fade whose cancellation is already under way.
                    DecodeEvent::Stopped => {}
                    // Nothing seeks the incoming track: it has no public identity
                    // until promotion; no command can name it.
                    DecodeEvent::SeekComplete { .. } => {}
                }
            }
        }
        if let Some(failure) = incoming_failed {
            crate::verr!(
                "[XFADE] the incoming track failed to decode ({}), falling back to a cut",
                failure.reason()
            );
            // Read before the cancellation, which consumes the record that names it.
            let failed_track = self
                .pending_crossfade
                .as_ref()
                .map(|s| s.next.track.clone());
            if self.cancel_crossfade_after_incoming_failure(failure) {
                // Too late for a cut: the callback swapped before this failure was drained,
                // and the dead track is the one playing. `promote_crossfade` leaves
                // `pending_complete` false, which is what carries this into the settle below.
                fatal_decode = true;
            } else if let Some(track) = failed_track {
                // Only THIS caller knows the staged copy is bad. The other five (load, stop,
                // seek, a device switch, the outgoing decoder dying) are the listener
                // changing their mind, and the bytes they leave staged are still good.
                // Cancelled path only: a promotion already spent that record.
                crate::audio::preload::discard_staged_if_match(&track);
            }
        } else if incoming_finished && let Some(pending) = self.pending_crossfade.as_mut() {
            crate::vprintln!("[XFADE] the incoming track ended inside the fade");
            pending.incoming_finished = true;
            // The same fact the callback needs, in the only form it can read. Without
            // it an empty incoming ring is indistinguishable from a late one, and the
            // envelope keeps advancing over silence to the end of its nominal length.
            if let Some(xfade) = self.cpal_xfade.as_ref() {
                xfade.in_eof.store(true, Relaxed);
            }
        }

        // Arming is decided HERE rather than in the decode thread, on the played cursor
        // rather than the decoded one. The decoder runs up to a full ring ahead of the
        // listener, and on a track shorter than the ring reaches the end within
        // milliseconds of starting, so a decode-side threshold fires far too early.
        // Running after the event drain above, a seek's rebase of `played_samples` is
        // always already applied, and the poll cadence can only make a fade start late.
        //
        // `!seeking` is load-bearing, not defensive: `played_samples` is not rebased until
        // the seek is acknowledged, so arming during one reads the position from BEFORE
        // it. A listener scrubbing back near the end would be crossfaded into the next
        // track seconds later, cutting off exactly what they rewound to hear.
        if self.crossfade_secs > 0
            && self.pending_crossfade.is_none()
            && self.is_playing
            && !self.seeking
            && self.current_duration > 0.0
        {
            let total =
                (self.current_duration * self.sample_rate as f64 * self.channels as f64) as u64;
            if crate::player::crossfade::crossfade_should_arm(
                self.played_samples.load(Relaxed),
                total,
                self.sample_rate,
                self.channels,
                self.crossfade_secs,
            ) {
                self.arm_crossfade();
            }
        }
        // The callback publishes a fresh generation once the fade is over. This is
        // the ordinary place that gets read, and a cancellation arriving first is
        // the other one.
        self.reconcile_completed_crossfade();

        // Without a settle the SDK stays stranded on its last reported state. Stopped maps to
        // NOT_PLAYING (the SDK's failed-to-resume contract), and `stop_decode` keeps a later
        // seek from muting against a dead channel. A paused player never reaches the
        // drain->Completed path mid-stream errors keep, and a dead decoder can no longer answer
        // a seek in flight: settle here, or the seek pin never clears.
        if fatal_decode
            && (decode_failure_needs_settle(
                self.pending_complete,
                self.played_samples.load(Relaxed),
            ) || !self.is_playing
                || self.seeking)
        {
            // The settle below calls `stop_decode`, which drops the cpal stream that a fade
            // in flight is also carrying the healthy incoming track through, its decode
            // thread owned by `pending_crossfade` with nothing else to retire it. Tear the
            // fade down first, or one track's decode error silences the other and leaks it.
            self.cancel_crossfade();
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
            self.governor.buffer_progress().set_playback_active(false);
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
            // A fade in flight owns this transition, and this counter cannot describe it:
            // `fade_pos` advances on the callback's own clock while `played_samples`
            // advances only by what the outgoing ring supplied. One underrun parts them for
            // good, and the counter can sit still with audio still queued. Read here as a
            // finished track it took the whole stream down mid-fade (`cancel_crossfade`,
            // then `stop_decode` dropping the cpal stream BOTH tracks play through),
            // leaving silence and a dead queue with 2.0s undelivered. A promotion is what
            // ends a faded track; the fade ends by its own swap or by a cancel, never here.
            // The comment below calls that unreachable, on a clamp this counter does not keep.
            if played > 0 && played == self.last_played_snapshot && self.pending_crossfade.is_some()
            {
                crate::vprintln!(
                    "[XFADE] the outgoing counter stalled at {played} (decoded={}) with a fade in flight; leaving the transition to the promotion",
                    self.decoded_samples.load(Relaxed),
                );
            } else if played > 0 && played == self.last_played_snapshot {
                crate::vprintln!(
                    "[DRAIN]  Ring buffer drained (played={}, decoded={})",
                    played,
                    self.decoded_samples.load(Relaxed),
                );
                self.pending_complete = false;
                self.last_played_snapshot = 0;
                // No fade can be in flight here, the branch above having taken that case, so
                // `stop_decode` below is free to drop the cpal stream. A played-out track owns
                // no resume position, and a same-URL reload must rebuild rather than resume in
                // place: decode EOF arrives up to a ring buffer early, and a pause there would
                // commit both against an unfinished track.
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
                self.governor.buffer_progress().set_playback_active(false);
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
