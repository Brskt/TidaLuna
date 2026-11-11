use super::decode::{DecodeThreadConfig, spawn_decode_thread};
use super::output::CrossfadeSlot;
use super::{CrossfadeState, DecodeCommand, PlayerThread};
use crate::player::{MediaErrorCode, MediaFormatSnapshot, PlayerEvent};
use std::sync::atomic::{
    AtomicU32, AtomicU64,
    Ordering::{Acquire, Relaxed},
};
use std::sync::mpsc;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Why the incoming decoder of a fade died. The two settle differently once the fade has
/// already promoted the track they describe; they travel apart instead of flattened
/// into a message: every `MediaError` code TIDAL maps advances the queue, and a track
/// whose bytes stopped arriving has not earned that.
pub(super) enum IncomingFailure {
    NetworkStalled,
    Decode(String),
}

impl IncomingFailure {
    /// What the log line reporting the lost fade names as the cause.
    pub(super) fn reason(&self) -> &str {
        match self {
            Self::NetworkStalled => "network stalled",
            Self::Decode(error) => error,
        }
    }
}

impl<F: Fn(PlayerEvent) + Send + 'static> PlayerThread<F> {
    pub(super) fn handle_set_crossfade_secs(&mut self, secs: u8) {
        // Takes effect at the next arming check. A fade already running
        // keeps the length it was armed with rather than changing mid-ramp.
        self.crossfade_secs = secs;
        // The governor sizes what a preload may draw through a shut gate from
        // this: a fade needs its own seconds of the incoming track, and the
        // gate is shut for most of a track whose download is paced at real time.
        self.governor.buffer_progress().set_crossfade_secs(secs);
        crate::vprintln!("[XFADE] overlap set to {secs}s");
    }

    /// Load a track's bytes straight from the disk cache, for a fade whose next
    /// track was never staged because the cache already held it.
    ///
    /// The READ is blocking and deliberately on the control thread: the decode threads and the
    /// audio callback are untouched, the ring already holds seconds of audio, and it happens
    /// once per transition. The LOOKUP does not. Arming polls four times a second for the whole
    /// tail of every track, and waiting on the cache mutex at that cadence turns any stall of its
    /// holders (a store's row, a step of an eviction or a Clear) into frozen transport.
    /// `ipc/window.rs` met the same mutex on the UI thread and answered it the same way.
    ///
    /// Busy therefore reads as a miss, which the caller already handles: not yet, ask again
    /// next tick, never a permanent refusal.
    fn read_cached_track(
        &self,
        track: &crate::state::TrackInfo,
    ) -> Option<crate::player::buffer::RamBuffer> {
        let id = crate::player::canonical_track_id(&track.url);
        let path = crate::state::AUDIO_CACHE
            .try_lock()
            .ok()
            .and_then(|cache| cache.lookup_path(&id))?;
        match crate::player::read_cache_entry(&path, &track.key) {
            Ok(data) => {
                crate::vprintln!(
                    "[XFADE] incoming track read from the cache ({})",
                    crate::player::format_bytes(data.len() as u64)
                );
                Some(crate::player::buffer::RamBuffer::from_complete(data))
            }
            // Leave the entry alone whatever the reason: the ordinary load path owns
            // the decision to drop or keep a bad one, and it will meet this same
            // entry moments later with the context to judge it. But SAY WHY, and
            // ungated. A cut where a fade was expected is otherwise indistinguishable
            // from the feature simply not working, and the reason is the diagnosis.
            Err(crate::player::CacheReadError::Orphaned) => {
                crate::verr!("[XFADE] no cached file behind the index row for {id}");
                None
            }
            Err(crate::player::CacheReadError::Unreadable(why)) => {
                crate::verr!("[XFADE] the cached entry could not be read: {why}");
                None
            }
            Err(crate::player::CacheReadError::Corrupt(why)) => {
                crate::verr!("[XFADE] the cached entry did not decrypt to audio: {why}");
                None
            }
        }
    }

    /// Start the incoming track alongside the outgoing one; the callback can then blend them.
    /// Never goes through `load_with_policy`: that mints a generation, aborts the previous
    /// load's task and cancels its download, which is exactly the track a fade has to keep
    /// alive. Every check fails closed, any doubt leaving today's hard cut in place.
    pub(super) fn arm_crossfade(&mut self) {
        if self.pending_crossfade.is_some() || self.crossfade_secs == 0 {
            return;
        }
        // Exclusive and ASIO carry no resampler and must release the device on a
        // format change: neither can host an overlap.
        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode || self.is_asio_mode {
                return;
            }
        }
        let Some(xfade) = self.cpal_xfade.clone() else {
            return;
        };

        // The fade can only be as long as the outgoing track still has audio to give. The
        // arming predicate is sticky, so a preload landing a second before the end would
        // otherwise arm the full configured length: the outgoing ring empties,
        // `played_samples` freezes, and the drain check reads that as a finished track,
        // tearing the stream down mid-fade. Clamping makes that unreachable.
        let per_second = self.sample_rate as u64 * self.channels as u64;
        let total_samples = (self.current_duration * per_second as f64) as u64;
        let remaining = total_samples.saturating_sub(self.played_samples.load(Relaxed));

        // The publication this fade leaves, read BEFORE the staged record is validated. A load
        // published after this read makes the promotion yield, the safe direction; read after
        // the validation, a load published in between would pass for the one being left.
        let armed_over = crate::state::CURRENT_TRACK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publication();

        // Where the next track's bytes come from. Two sources, not one: `start_preload` SKIPS
        // the fetch when the disk cache already holds the track, so asking the staged copy
        // alone meant a fade could never arm on a cached library. Validated on a BORROW before
        // consuming, because taking first destroyed the staged track on every refusal and left
        // the completion path nothing to auto-load. The generation comes out of this check.
        let Some(crate::state::RetainedTrack {
            track: next,
            load_gen,
        }) = crate::audio::preload::peek_next_track()
        else {
            return;
        };
        // Refusing is final for this staged track. The arming predicate stays true
        // once it trips. Re-deciding every poll tick would repeat the work for
        // the whole tail of the track and log the same refusal dozens of times.
        if self.xfade_refused_url.as_deref() == Some(next.url.as_str()) {
            return;
        }
        // Decided here, once the track is known, to let a refusal be recorded: the
        // arming predicate stays true for the rest of the track, and an unrecorded
        // refusal repeats on every poll tick.
        let Some(mut len_samples) =
            crate::player::crossfade::fade_len_samples(per_second, self.crossfade_secs, remaining)
        else {
            crate::vprintln!(
                "[XFADE] only {:.2}s of the track left, falling back to a cut",
                remaining as f64 / per_second.max(1) as f64
            );
            self.xfade_refused_url = Some(next.url);
            return;
        };
        let peek = match crate::audio::preload::peek_preloaded() {
            Some(peek) => peek,
            None => {
                // Read it off disk here rather than teaching the preload to stage
                // cached tracks: this keeps the bytes off the heap for everyone who
                // has crossfade off, and the read lands on the control thread, never
                // on the audio callback or the decoder.
                let Some(buffer) = self.read_cached_track(&next) else {
                    // The one refusal that used to be silent while the three below all
                    // announce themselves, and the most common: it is where a track
                    // whose head has not landed yet, and which is not on disk, ends up.
                    if self.xfade_waiting_url.as_deref() != Some(next.url.as_str()) {
                        self.xfade_waiting_url = Some(next.url.clone());
                        crate::vprintln!(
                            "[XFADE] the next track is neither staged nor cached, waiting"
                        );
                    }
                    // NOT recorded as a refusal: the preload publishes its buffer as
                    // soon as a head lands, so "nothing staged" now means "not yet" as
                    // often as "never". The refusals below stay permanent, describing a
                    // track that will not work rather than one that is not ready.
                    return;
                };
                crate::audio::preload::PeekedTrack {
                    track: next,
                    buffer,
                }
            }
        };
        // The incoming decoder always conforms to the stream's rate: the fade
        // itself is never the problem. The guard below is about the aftermath: with a
        // per-track stream, a differing native rate would leave the promoted track
        // stuck at the outgoing track's rate, which is why it still refuses where the
        // rate is not pinned to the device.
        let (duration, media_format) =
            match crate::player::thread::output::probe_audio_format(&peek.buffer) {
                Ok(info)
                    if crate::player::crossfade::crossfade_accepts_rate(
                        info.sample_rate,
                        self.sample_rate,
                        crate::player::thread::output::ENGINE_RATE_IS_PINNED,
                    ) =>
                {
                    (
                        info.duration,
                        Some(crate::player::MediaFormatSnapshot {
                            codec: info.codec,
                            sample_rate: info.sample_rate,
                            output_sample_rate: self.sample_rate,
                            bit_depth: info.bit_depth,
                            channels: info.channels,
                            bytes: peek.buffer.total_len(),
                        }),
                    )
                }
                Ok(info) => {
                    crate::vprintln!(
                        "[XFADE] rate mismatch {} vs {}, falling back to a cut",
                        info.sample_rate,
                        self.sample_rate
                    );
                    self.xfade_refused_url = Some(peek.track.url);
                    return;
                }
                Err(e) => {
                    crate::vprintln!("[XFADE] probe failed ({e}), falling back to a cut");
                    self.xfade_refused_url = Some(peek.track.url);
                    return;
                }
            };
        // The head, not the whole track, is what a fade consumes, and the staged buffer is
        // published while still filling: what has landed NOW is the real limit. Refusing
        // instead made a 71 MB track a hard cut for want of the 1.2 MB six seconds of CD
        // FLAC uses. Bytes per second comes from the track's own size and duration.
        len_samples = crate::player::crossfade::fade_len_from_staged(
            len_samples,
            peek.buffer.written(),
            peek.buffer.total_len(),
            duration,
            per_second,
        );
        if len_samples < per_second as usize {
            // Under a second is not a fade. NOT recorded as a refusal: the bytes are
            // still arriving, and the next poll tick may well have enough. Announced
            // once for the same reason as the wait above: the poll runs four times a
            // second for the whole tail of the track.
            if self.xfade_waiting_url.as_deref() != Some(peek.track.url.as_str()) {
                self.xfade_waiting_url = Some(peek.track.url.clone());
                crate::vprintln!(
                    "[XFADE] only {:.2}s of the incoming track buffered, waiting",
                    len_samples as f64 / per_second.max(1) as f64
                );
            }
            return;
        }

        let buffer = peek.buffer;
        let track = peek.track;

        let ring_size = self.sample_rate as usize * self.channels as usize * 2;
        let (producer, consumer) = rtrb::RingBuffer::new(ring_size);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let decoded = Arc::new(AtomicU64::new(0));
        let reader_cancel = Arc::new(AtomicBool::new(false));

        let Some(handle) = spawn_decode_thread(DecodeThreadConfig {
            buffer: buffer.clone(),
            producer,
            decoded_samples: decoded.clone(),
            cmd_rx,
            event_tx,
            output_rate: self.sample_rate,
            output_channels: self.channels,
            seek_gen: Arc::new(AtomicU32::new(0)),
            reader_cancel: reader_cancel.clone(),
        }) else {
            self.xfade_refused_url = Some(track.url);
            return;
        };

        // A decode thread starts PAUSED and produces nothing until this arrives.
        // It has to be sent now, not at promotion: the callback needs real audio in
        // the incoming ring throughout the overlap, or the "crossfade" is a plain
        // fade to silence. Safe here because the incoming decoder is fully isolated
        // in its own ring and counters, touching nothing the outgoing track reads.
        if cmd_tx.send(DecodeCommand::Resume).is_err() {
            crate::verr!("[XFADE] the incoming decoder died before it could start");
            reader_cancel.store(true, Relaxed);
            let _ = handle.join();
            return;
        }

        // The staged record is deliberately NOT consumed here. Arming is not the
        // transition: a skip, a seek, a device switch or the incoming decoder failing all
        // leave the outgoing track playing to its real end, and consumed at arm time the
        // completion that followed found nothing staged and stopped dead with the queue
        // intact. It is spent at promotion instead, and nothing is double-served meanwhile,
        // the fade holding an `Arc` clone of the same bytes.

        // Seeded from the current state rather than cleared: the outgoing decoder
        // runs ahead and may already have parked at EOF before the fade arms. Reset
        // to false would then be a lie the callback waits on forever.
        xfade.out_eof.store(self.pending_complete, Relaxed);
        // Cleared, unlike `out_eof` above: this decoder is spawned by this arm and
        // cannot already have finished. A stale `true` from the previous fade would
        // make the callback shorten this one against a full ring.
        xfade.in_eof.store(false, Relaxed);
        *xfade.attach.lock().unwrap_or_else(|e| e.into_inner()) = Some(CrossfadeSlot {
            consumer,
            len_samples,
        });
        crate::vprintln!(
            "[XFADE] armed, {:.2}s overlap (setting {}s)",
            len_samples as f64 / per_second.max(1) as f64,
            self.crossfade_secs
        );

        self.pending_crossfade = Some(CrossfadeState {
            cmd_tx,
            event_rx,
            handle: Some(handle),
            reader_cancel,
            decoded,
            next: crate::state::RetainedTrack { track, load_gen },
            armed_over,
            buffer,
            duration,
            media_format,
            incoming_finished: false,
        });
    }

    /// Make the incoming track the current one. Runs once, at the end of the fade, the only
    /// instant at which any observer sees the change.
    ///
    /// The rings need nothing here: the callback already swapped the incoming consumer into
    /// its primary slot, owning both by move. This moves identity, and nothing else.
    /// `in_played` is the new track's position, not an offset into the stream's lifetime total.
    pub(super) fn promote_crossfade(&mut self, in_played: u64) {
        let Some(mut state) = self.pending_crossfade.take() else {
            return;
        };

        // Retire the outgoing decoder. Same sequence as `stop_decode`, minus its
        // teardown of the cpal stream, which must survive the promotion. The
        // `wake_readers` is not optional: a decoder starved at the download
        // frontier is parked in a read that only re-checks its cancel flag every
        // 5 s, and the join below runs on this thread.
        if let Some(tx) = self.decode_cmd_tx.take() {
            let _ = tx.send(DecodeCommand::Stop);
        }
        super::retire_reader(&mut self.decode_reader_cancel, self.current_buffer.as_ref());
        if let Some(handle) = self.decode_handle.take() {
            let _ = handle.join();
        }

        // The outgoing track was heard to its end; it owns no resume point any
        // more. Every other "this track is done" path clears this too.
        if let Some(ref prev) = self.current_track_id {
            self.resume_store.clear(prev);
        }

        self.decode_cmd_tx = Some(state.cmd_tx);
        self.decode_event_rx = Some(state.event_rx);
        self.decode_handle = state.handle.take();
        self.decode_reader_cancel = Some(state.reader_cancel);
        self.decoded_samples = state.decoded;
        self.set_current_buffer(state.buffer);

        // The clock is deliberately NOT rebased here. Only the tick that swapped the rings
        // knows where the swap fell: by the time this thread reads `done` the counter holds
        // the outgoing total plus whatever the promoted ring has since delivered, so any
        // store made here discards audio already heard. `output/mixer.rs` fixes the base instead.
        self.current_duration = state.duration;

        // Clear the completion state, exactly as a load does. It is GUARANTEED to be set
        // here: the swap waits on `out_eof`, raised only by the outgoing decoder's own
        // `Finished`, which sets these two. Carried onto the incoming track they suppress
        // every resume-store write for it and turn two stalled ticks into a false completion.
        self.pending_complete = false;
        self.last_played_snapshot = 0;

        // The governor's totals describe whichever track is current. `written` and
        // `read_pos` are refreshed by the poll loop anyway, but `total_len` and
        // `bitrate_bps` are only ever written by a load. After a fade they stay
        // pinned to the outgoing track and feed real throttle, starvation and
        // preload-gate decisions with the wrong track's numbers.
        if let Some(ref buf) = self.current_buffer {
            let total_len = buf.total_len();
            let bitrate_bps = if self.current_duration > 0.0 {
                (total_len as f64 / self.current_duration) as u64
            } else {
                0
            };
            let bp = self.governor.buffer_progress();
            bp.bitrate_bps.store(bitrate_bps, Relaxed);
            bp.total_len.store(total_len, Relaxed);
            bp.written.store(buf.written(), Relaxed);
            bp.read_pos.store(buf.read_cursor(), Relaxed);
            // Overwriting the totals is not enough to announce a new track: the governor's
            // per-track state was renewed by them returning to zero, which only a load
            // does. Without this the incoming track inherits the head allowance the
            // outgoing one had already spent, and the fade after this one gets no bytes.
            bp.begin_track();
        }

        // The canonical id is derived from the url, the same way every load does it;
        // `TrackInfo` carries no id of its own.
        let track_id = crate::player::canonical_track_id(&state.next.track.url);
        self.current_track_id = Some(track_id.clone());
        self.current_product_id = state.next.track.product_id.clone();
        self.current_format = state.next.track.format.clone();
        // Without this, `Player::load`'s idempotent reconcile still names the
        // outgoing track. The frontend re-issuing load() for the track we just
        // faded into then reads as a different track and forces a full rebuild, audibly
        // undoing the fade.
        self.set_committed_track(Some((track_id, state.next.track.format.clone())));
        // A preloaded track is a fresh network fetch, never a disk-cache hit. None
        // is the safe direction: it lets the completion path store it, which is
        // idempotent, rather than skip a track that was never cached.
        self.cached = None;
        // This pair is ordered, and the order is the fix. `commit_peeked` flips the staged
        // buffer's owner to `Playback`, and from that instant a download restart resolves its
        // url through `CURRENT_TRACK`. Published second, a restart landing in between reads the
        // OUTGOING track, fails the identity check, and silently abandons the download feeding
        // the one that just became audible, discovered 30s later as a stall. Published as the
        // pair the fade was handed: a promotion is not a load and bumps nothing.
        let verdict = crate::state::CURRENT_TRACK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publish_promotion(&state.next, state.armed_over);
        // NOW the staged record is spent, because only now has the track it names
        // actually become current. `arm_crossfade` used to do this, and a fade that
        // was cancelled afterwards left the completion path with nothing to advance
        // to. Both fields clear on identity inside; a preload that staged a
        // DIFFERENT track during the overlap keeps its bytes.
        crate::audio::preload::commit_peeked(&state.next.track);

        if let Some(snapshot) = state.media_format {
            self.last_media_format = Some(snapshot);
        }
        // A track shorter than the fade ended during the overlap, and the fade's drain consumed
        // its `Finished`. Replay that here, now the track is current, or it never completes and
        // never reaches the disk cache.
        if state.incoming_finished {
            self.store_finished_ciphertext();
            self.pending_complete = true;
            self.last_played_snapshot = self.played_samples.load(Relaxed).wrapping_sub(1);
            crate::vprintln!("[XFADE] promoted track had already finished, awaiting its drain");
        }

        match verdict {
            crate::state::PromotionVerdict::Published => {
                self.announce_promotion(in_played, state.duration, state.media_format);
            }
            crate::state::PromotionVerdict::Superseded => {
                crate::vprintln!(
                    "[XFADE] promoted under a newer load; announcing it is that load's job"
                );
            }
        }
    }

    /// What the SDK and Connect hear about a promotion. Withheld under a newer load: whoever
    /// asked for it has already left the transition this `completed` would announce.
    ///
    /// Announced under the seq it inherits, never a fresh one. Every seq is minted by the load or
    /// stop that owns it, on its requester's thread, before this thread handles that command;
    /// minted here, a promotion would outrank a command already in flight, and the renderer drops
    /// every event below the highest it has seen.
    fn announce_promotion(
        &mut self,
        in_played: u64,
        duration: f64,
        media_format: Option<MediaFormatSnapshot>,
    ) {
        crate::vprintln!(
            "[XFADE] promoted, the incoming track is now current ({in_played} samples in)"
        );
        // The incoming track's position, reported in the same breath as the transition.
        // `handleAutomaticTransitionToPreloadedMediaProduct` is the one SDK transition
        // that assigns no `currentTime` of its own, so until a report replaces it every
        // surface still holds the OUTGOING position. Left to the periodic tick it is held
        // twice over, by a 24 ms flush debounce and a 250 ms renderer throttle armed by
        // the outgoing track's last tick. Emitted here it is already pending when
        // `completed` below forces the flush, and both travel in one batch, one JS call.
        //
        // It lands AFTER `completed` there, and the order is LOAD-BEARING: the pending
        // position is appended behind the queued events, and the SDK's `mediastate`
        // listener synchronously calls `finishCurrentMediaProduct`, which reads
        // `endAssetPosition: this.currentTime` for the track that just ended. Deliver the
        // position first and the outgoing track's play statistics report the incoming
        // track's. Identity first, position second: do not "correct" either into leading.
        //
        // Clamped the way the periodic emitter clamps: `state.duration` is a probe
        // estimate, and the overlap is bounded by what remained of the OUTGOING track. A
        // container that undercounts its frames can drain past the duration it declared.
        let position = self.played_position_secs();
        (self.callback)(PlayerEvent::TimeUpdate(
            if self.current_duration > 0.0 {
                position.min(self.current_duration)
            } else {
                position
            },
            self.current_seq,
        ));
        // THREE events, in this order, and the order is a contract TIDAL's own
        // transition handler imposes:
        //
        //   handleAutomaticTransitionToPreloadedMediaProduct() {
        //       await this.nativeEvent(`mediaduration`);   // it blocks HERE
        //       ...
        //       await this.mediaStateChange(`active`);     // then HERE
        //       dispatchEvent(mediaProductTransition)      // only then does the UI move
        //   }
        //
        // `completed` is what makes it enter that handler; without the duration it waits
        // forever and every surface keeps naming the outgoing track while the incoming one
        // plays. The duration looks unnecessary because it does NOT drive the OS media
        // controls (`settle_measured_duration` matches it against metadata still naming the
        // outgoing track). True, and beside the point: what needs it is the SDK's await.
        (self.callback)(PlayerEvent::CrossfadePromoted(
            self.current_seq,
            self.current_product_id.clone(),
        ));
        (self.callback)(PlayerEvent::Duration(
            duration,
            self.current_seq,
            self.current_product_id.clone(),
        ));
        (self.callback)(PlayerEvent::StateChange(
            crate::player::PlaybackState::Active,
            self.current_seq,
        ));

        // The format badge is separate: it is also re-sent verbatim on a re-assert,
        // left alone it keeps describing the outgoing track's codec and bit
        // depth. The snapshot comes from the probe arming already paid for.
        if let Some(snapshot) = media_format {
            (self.callback)(snapshot.to_event());
        }
    }

    /// Adopt a fade the callback has already finished but nobody has acted on yet,
    /// and report whether it promoted.
    ///
    /// The callback ends a fade on its own clock, swapping its consumer and stamping `done`
    /// inside one tick, while everything that reads that stamp runs on this thread. Reading it
    /// from one place keeps the answer identical whichever caller gets there first.
    pub(super) fn reconcile_completed_crossfade(&mut self) -> bool {
        let Some(word) = self.cpal_xfade.as_ref().map(|x| x.done.load(Acquire)) else {
            return false;
        };
        let Some(completed) = self.fresh_completion(word) else {
            return false;
        };
        self.adopt_completion(completed);
        true
    }

    /// The `(generation, origin)` in `word` if it names a fade nobody has acted on yet. A read
    /// with no effect: the teardown takes it under the offer guard and acts after it.
    fn fresh_completion(&self, word: u64) -> Option<(u16, u64)> {
        let (cur_gen, origin) = crate::player::thread::output::unpack_xfade_done(word);
        (cur_gen != 0 && cur_gen != self.xfade_seen_gen).then_some((cur_gen, origin))
    }

    fn adopt_completion(&mut self, (cur_gen, origin): (u16, u64)) {
        self.xfade_seen_gen = cur_gen;
        self.promote_crossfade(origin);
    }

    /// Drop the incoming track and detach it from the callback. The cleanup path for a
    /// listener changing their mind (a skip, a seek, a stop, a device switch) and for
    /// the OUTGOING decoder dying.
    ///
    /// Every caller here leaves good bytes staged, so a fade the callback already finished may
    /// simply promote and carry the caller's intent onto the track now playing. That is wrong
    /// when the INCOMING track is what failed, which
    /// `cancel_crossfade_after_incoming_failure` names.
    pub(super) fn cancel_crossfade(&mut self) {
        self.tear_down_crossfade();
    }

    /// Cancel a fade because the incoming track's own decoder died, and report whether the
    /// player must now settle fatally.
    ///
    /// True means the callback had already swapped before the failure was drained: the track
    /// that just died is the one playing, behind a decode thread that has already returned.
    /// Nothing else would ever report it, so the failure is published here, through the same
    /// events an outgoing decoder's death publishes.
    #[must_use]
    pub(super) fn cancel_crossfade_after_incoming_failure(
        &mut self,
        failure: IncomingFailure,
    ) -> bool {
        if !self.tear_down_crossfade() {
            return false;
        }
        match failure {
            IncomingFailure::NetworkStalled => {
                (self.callback)(PlayerEvent::NetworkLost(self.current_seq));
            }
            IncomingFailure::Decode(error) => (self.callback)(PlayerEvent::MediaError {
                error,
                code: MediaErrorCode::UnreadableFile,
                seq: self.current_seq,
                product_id: None,
            }),
        }
        // Left set, both arm a guard nothing can clear: the channel behind them is dead.
        self.set_committed_track(None);
        self.decode_cmd_tx = None;
        true
    }

    /// The teardown both cancellations share, reporting whether the fade was promoted
    /// instead of cancelled.
    ///
    /// A finished fade cannot be cancelled: the callback swapped its consumer, so retiring the
    /// incoming decoder would leave the device draining a ring nobody fills, with no watchdog
    /// behind it since the stall detector watches the healthy outgoing buffer.
    ///
    /// The verdict is taken under the offer guard, the one the callback swaps under. Read outside
    /// it, the callback could finish the fade between the read and the kill below, which would
    /// then retire the decoder feeding the ring just promoted. Only the read and the veto sit
    /// inside the guard; both joins run after it drops.
    fn tear_down_crossfade(&mut self) -> bool {
        let mut completed = None;
        let mut retracted = None;
        if let Some(xfade) = self.cpal_xfade.as_ref() {
            let mut attach = xfade.attach.lock().unwrap_or_else(|e| e.into_inner());
            completed = self.fresh_completion(xfade.done.load(Acquire));
            if completed.is_none() && self.pending_crossfade.is_some() {
                // Retract an offer the callback has not taken yet...
                retracted = attach.take();
                // ...and release one it already has. Two different states, two
                // signals: `attach == None` alone cannot say which.
                xfade.cancel.store(true, Relaxed);
            }
        }
        // Outside the guard: a retracted ring whose producer already died is freed here, and the
        // callback's `try_lock` must not meet that free.
        drop(retracted);
        if let Some(completed) = completed {
            self.adopt_completion(completed);
            return true;
        }
        let Some(mut state) = self.pending_crossfade.take() else {
            return false;
        };
        let _ = state.cmd_tx.send(DecodeCommand::Stop);
        state.reader_cancel.store(true, Relaxed);
        state.buffer.wake_readers();
        if let Some(handle) = state.handle.take() {
            let _ = handle.join();
        }
        crate::vprintln!("[XFADE] cancelled");
        false
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/player/thread/crossfade.rs"]
mod tests;
