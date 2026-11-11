use super::{DecodeCommand, PlayerThread};
use crate::player::{PlaybackState, PlayerCommand, PlayerEvent};
use std::sync::atomic::Ordering::Relaxed;

use cpal::traits::StreamTrait;

#[cfg(target_os = "windows")]
use crate::player::asio::host::AsioCommand;
#[cfg(target_os = "windows")]
use crate::player::wasapi::ExclusiveCommand;

/// The defined outcomes of a `player.play`, given the player's current state.
/// Pure, to be unit-tested without the audio pipeline.
#[derive(Debug, PartialEq)]
pub(super) enum PlayAction {
    /// A live pipeline exists: resume it.
    Resume,
    /// A load for this generation is genuinely in flight: wait for it.
    DeferTo(u32),
    /// No load is coming, but a previously-loaded source is retained: reload it.
    ReArm,
    /// Nothing is loaded and nothing to re-arm (cold/empty): do nothing.
    Ignore,
}

/// Decide what a `player.play` does. Deferring is legitimate ONLY while a load
/// is in flight; otherwise a no-track play re-arms the retained source.
pub(super) fn decide_play(
    has_track: bool,
    loading_gen: Option<u32>,
    has_retained_source: bool,
) -> PlayAction {
    match (has_track, loading_gen, has_retained_source) {
        (true, _, _) => PlayAction::Resume,
        (false, Some(generation), _) => PlayAction::DeferTo(generation),
        (false, None, true) => PlayAction::ReArm,
        (false, None, false) => PlayAction::Ignore,
    }
}

impl<F: Fn(PlayerEvent) + Send + 'static> PlayerThread<F> {
    pub(super) fn stop_decode(&mut self) {
        if let Some(tx) = self.decode_cmd_tx.take() {
            let _ = tx.send(DecodeCommand::Stop);
        }
        // A decoder starved at the download frontier is parked in its read, not on that
        // channel, and the join below runs on this thread: retire its reader and the read
        // returns now. Per-reader, keeping alive a buffer the caller is about to hand to
        // the next decoder, which cancelling the buffer would not.
        if let Some(cancel) = self.decode_reader_cancel.take() {
            cancel.store(true, Relaxed);
            if let Some(ref buf) = self.current_buffer {
                buf.wake_readers();
            }
        }
        self.cpal_stream = None;
        if let Some(handle) = self.decode_handle.take() {
            let _ = handle.join();
        }
        self.decode_event_rx = None;
        // Nothing survives to answer a seek sent to the thread just retired, and a pin left
        // here follows the next track: pinned progress bar, suppressed resume writes.
        self.seeking = false;
        self.seek_target = None;
        self.seek_wall_start = None;
    }

    /// Mint the identity of a new seek. Every dispatch goes through here, live or folded
    /// into a decoder spawn, which is why there is no per-stream reset to forget: the
    /// counter only ever moves forward, putting every earlier ack permanently out of reach.
    pub(super) fn next_seek_gen(&mut self) -> u32 {
        self.seek_ack_gen = self.seek_ack_gen.wrapping_add(1);
        self.seek_ack_gen
    }
    pub(super) fn handle_play(&mut self) {
        if self.orphaned {
            crate::vprintln!("[PLAY]   ignored: the page that loaded this track is gone");
            return;
        }
        self.allow_startup_auto_resume = false;
        // Resuming cancels a pending real-stop device release (the user came back).
        #[cfg(target_os = "windows")]
        {
            self.exclusive_release_at = None;
            self.asio_release_at = None;
        }

        // Capture the retained source only when no track and no load in flight;
        // the short-circuit skips the CURRENT_TRACK lock on the resume path.
        let retained = if !self.has_track && self.loading_gen.is_none() {
            crate::state::CURRENT_TRACK
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retained()
                .cloned()
        } else {
            None
        };
        match decide_play(self.has_track, self.loading_gen, retained.is_some()) {
            PlayAction::DeferTo(generation) => {
                // A load is genuinely in flight: handle_load applies this play
                // when it delivers for the matching generation.
                self.pending_play = Some(generation);
                crate::vprintln!(
                    "[PLAY]   deferred until load #{generation} is ready (load in flight)"
                );
                return;
            }
            PlayAction::ReArm => {
                // No load coming but a source is retained: hand the captured
                // track to flush.rs (avoids a second CURRENT_TRACK lock).
                crate::vprintln!("[PLAY]   no live pipeline; re-arming retained source");
                if let Some(retained) = retained {
                    let track = retained.track;
                    // Resume at the retained source's last position, not 0 (e.g. a >10s
                    // pause released the device, re-arming here). resume_store holds it
                    // but is cleared on track-end; a post-Completed re-arm gets None
                    // and starts at 0; the mode's own live position is the floor-free
                    // fast-path.
                    let replayed = crate::player::canonical_track_id(&track.url);
                    let position = self.resume_store.get(&replayed);
                    #[cfg(target_os = "windows")]
                    let position = if self.is_asio_mode {
                        self.last_asio_pos.or(position)
                    } else {
                        self.last_exclusive_pos.or(position)
                    };
                    // A seek taken while the device was released outranks both the live
                    // mirror and the store: it names where the user asked to be. Read rather
                    // than taken, since the load this replay triggers is what consumes it.
                    #[cfg(target_os = "windows")]
                    let position = self
                        .user_seek_override
                        .as_ref()
                        .filter(|(id, _)| *id == replayed)
                        .map(|(_, time)| *time)
                        .or(position);
                    (self.callback)(PlayerEvent::ReplayRequest {
                        track,
                        expected_gen: retained.load_gen,
                        position,
                        play: true,
                    });
                }
                return;
            }
            PlayAction::Ignore => {
                crate::vprintln!("[PLAY]   play with no track and no retained source; ignoring");
                return;
            }
            // Fall through to the live-pipeline resume below.
            PlayAction::Resume => {}
        }
        self.pending_play = None;

        #[cfg(target_os = "windows")]
        {
            if self.is_asio_mode {
                if let Some(seek_time) = self.take_start_position() {
                    // Announce the position something actually took, as handle_seek does:
                    // a queued seek can outlive its decoder, and a park leaves none at all.
                    let target = seek_time.max(0.0);
                    let gen_id = self.next_seek_gen();
                    let dispatched = self
                        .asio_seek_tx
                        .as_ref()
                        .is_some_and(|tx| tx.send((seek_time, gen_id)).is_ok());
                    if dispatched {
                        self.last_asio_pos = Some(target);
                        (self.callback)(PlayerEvent::TimeUpdate(target, self.current_seq));
                    } else if self.asio_handle.is_some()
                        && let Some(buffer) = self.current_buffer.clone()
                    {
                        // A respawn carries the position, making the announced target reachable.
                        self.last_asio_pos = Some(target);
                        self.spawn_asio_decoder(buffer, Some(seek_time), false);
                        (self.callback)(PlayerEvent::TimeUpdate(target, self.current_seq));
                    } else {
                        // Nothing took it, and spawn_asio_decoder no-ops without a handle.
                        let live = self.last_asio_pos.unwrap_or(0.0);
                        (self.callback)(PlayerEvent::TimeUpdate(live, self.current_seq));
                        crate::vprintln!(
                            "[PLAY]   asio: no decoder took the queued seek; position restored"
                        );
                    }
                } else if let Some(ref handle) = self.asio_handle {
                    // Only resume the live retained stream. A missing id means there
                    // is no retained stream to resume; sending id 0 would be silently
                    // dropped by the stream-scoped guard; log it instead.
                    match self.current_asio_stream_id {
                        Some(stream_id) => {
                            handle.send(AsioCommand::Play { stream_id });
                        }
                        None => {
                            crate::vprintln!("[ASIO]   play: no live stream id; nothing to resume");
                        }
                    }
                } else {
                    // Still in ASIO mode with the handle gone: a device switch parked behind the
                    // driver teardown owns the next one. `is_playing` below is the only record of
                    // this Play, read back by the parked switch to decide whether to resume. Loud:
                    // until that reap lands the transport reports Active with nothing feeding it.
                    crate::verr!("[ASIO]   play: handle released, waiting on the parked switch");
                }
                self.is_playing = true;
                self.governor.buffer_progress().set_playback_active(true);
                return;
            }
        }

        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                if let Some(seek_time) = self.take_start_position() {
                    // Announce the position something actually took, as handle_seek does:
                    // a decoder that died without reporting leaves a sender that accepts
                    // nothing, and presence alone reads as success.
                    let target = seek_time.max(0.0);
                    let gen_id = self.next_seek_gen();
                    let dispatched = self
                        .exclusive_seek_tx
                        .as_ref()
                        .is_some_and(|tx| tx.send((seek_time, gen_id)).is_ok());
                    if dispatched {
                        self.last_exclusive_pos = Some(target);
                        (self.callback)(PlayerEvent::TimeUpdate(target, self.current_seq));
                    } else if self.exclusive_handle.is_some()
                        && let Some(buffer) = self.current_buffer.clone()
                    {
                        // A respawn carries the position, making the announced target reachable.
                        self.last_exclusive_pos = Some(target);
                        self.spawn_exclusive_decoder(buffer, Some(seek_time), false);
                        (self.callback)(PlayerEvent::TimeUpdate(target, self.current_seq));
                    } else {
                        // Nothing took it, and spawn_exclusive_decoder no-ops without a handle.
                        let live = self.last_exclusive_pos.unwrap_or(0.0);
                        (self.callback)(PlayerEvent::TimeUpdate(live, self.current_seq));
                        crate::vprintln!(
                            "[PLAY]   exclusive: no decoder took the queued seek; position restored"
                        );
                    }
                } else if let Some(ref handle) = self.exclusive_handle {
                    // Only resume the live adopted stream (mirrors the ASIO branch):
                    // a missing id means there is no stream to resume yet.
                    match self.current_exclusive_stream_id {
                        Some(stream_id) => {
                            handle.send(ExclusiveCommand::Play { stream_id });
                        }
                        None => {
                            crate::vprintln!("[WASAPI] play: no live stream id; nothing to resume");
                        }
                    }
                }
                self.is_playing = true;
                self.governor.buffer_progress().set_playback_active(true);
                return;
            }
        }

        self.is_playing = true;
        self.governor.buffer_progress().set_playback_active(true);
        (self.callback)(PlayerEvent::StateChange(
            PlaybackState::Active,
            self.current_seq,
        ));

        // The intent is spent either way. Announce where the reader landed, never the request:
        // a refused pre-seek left the decoder where it was.
        self.pending_resume_seek = None;
        if let Some(landed) = self.pre_seek_pos {
            (self.callback)(PlayerEvent::TimeUpdate(landed.max(0.0), self.current_seq));
            crate::vprintln!("[PLAY]   start at pre-seeked {:.1}s", landed);
        } else {
            // Nothing landed says nothing about where the decoder sits: a resume after a
            // paused seek starts at that target, not at zero.
            crate::vprintln!("[PLAY]   start at {:.1}s", self.effective_position());
        }
        // A prior device-loss recovery may have torn down the stream (e.g. the device
        // was held exclusively by a fullscreen game). Resuming is a natural retry point:
        // rebuild on the current default device, which is usually free again by now.
        if self.cpal_stream.is_none() {
            self.recover_audio_device();
        } else {
            self.start_playback();
        }
    }

    pub(super) fn start_playback(&mut self) {
        // Both fields are set and cleared together everywhere, and both callers reach here only with
        // a stream: the old `else` arms were dead; a missing stream already surfaces as a
        // `DeviceError` from `recover_audio_device`, which runs instead of this. `stream.play()`
        // failing is the one real failure left and nothing else reports it. It is logged ungated.
        if let Some(ref stream) = self.cpal_stream {
            match stream.play() {
                Ok(()) => crate::vprintln!("[PLAY]   cpal stream.play() OK"),
                Err(e) => crate::verr!("[ERROR]  cpal stream.play() failed: {e}"),
            }
        }
        if let Some(ref tx) = self.decode_cmd_tx {
            let _ = tx.send(DecodeCommand::Resume);
            crate::vprintln!("[PLAY]   DecodeCommand::Resume sent");
        }
        self.pre_seek_pos = None;
    }

    pub(super) fn try_skip_pre_seek(&mut self, target: f64) -> bool {
        if let Some(pre_pos) = self.pre_seek_pos.take()
            && (pre_pos - target).abs() < super::PRE_SEEK_TOLERANCE
        {
            (self.callback)(PlayerEvent::TimeUpdate(target.max(0.0), self.current_seq));
            return true;
        }
        false
    }

    pub(super) fn handle_pause(&mut self) {
        // A pause cancels any play deferred while the track was still loading;
        // otherwise handle_load would auto-play it against this pause intent.
        self.pending_play = None;
        // User pause: a same-track re-assert must not auto-resume. Set above the
        // windows early-returns, covering every mode (see resume_on_reassert).
        self.resume_on_reassert = false;
        // A pause is not a stop; handle_stop overwrites this after calling here. Ahead of
        // the windows early-returns like resume_on_reassert, letting every mode record it.
        self.idle_state = PlaybackState::Paused;
        #[cfg(target_os = "windows")]
        {
            if self.is_asio_mode {
                let was_playing = self.is_playing;
                if let Some(ref handle) = self.asio_handle {
                    match self.current_asio_stream_id {
                        Some(stream_id) => {
                            handle.send(AsioCommand::Pause { stream_id });
                        }
                        None => {
                            crate::vprintln!("[ASIO]   pause: no live stream id; nothing to pause");
                        }
                    }
                }
                self.is_playing = false;
                // ASIO pause keeps the driver clock, but its exclusive claim blocks
                // other apps: release it on a sustained pause. A short pause/resume
                // or a stop->load cancels this in time (handle_play/handle_load);
                // resume after a release respawns the driver (self-heal).
                if was_playing && self.asio_handle.is_some() {
                    self.asio_release_at =
                        Some(std::time::Instant::now() + super::ASIO_IDLE_RELEASE);
                }
                self.governor.buffer_progress().set_playback_active(false);
                self.resume_store.flush_if_due(true);
                return;
            }
        }

        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                let was_playing = self.is_playing;
                if let Some(ref handle) = self.exclusive_handle {
                    match self.current_exclusive_stream_id {
                        Some(stream_id) => {
                            handle.send(ExclusiveCommand::Pause { stream_id });
                        }
                        None => {
                            crate::vprintln!("[WASAPI] pause: no live stream id; nothing to pause");
                        }
                    }
                }
                self.is_playing = false;
                // Hold the device, then release it on a sustained pause, letting other
                // apps regain the DAC (TIDAL has no stop button -> pause is the stop signal).
                // A short pause/resume or a track-change stop->load cancels this in time
                // (handle_play/handle_load): only a real lingering pause fires.
                if was_playing {
                    self.exclusive_release_at =
                        Some(std::time::Instant::now() + super::EXCLUSIVE_PAUSE_RELEASE);
                }
                self.governor.buffer_progress().set_playback_active(false);
                self.resume_store.flush_if_due(true);
                return;
            }
        }

        if let Some(ref stream) = self.cpal_stream {
            let _ = stream.pause();
        }
        if let Some(ref tx) = self.decode_cmd_tx {
            let _ = tx.send(DecodeCommand::Pause);
        }

        let pos_secs = self.effective_position();
        (self.callback)(PlayerEvent::TimeUpdate(pos_secs, self.current_seq));

        self.is_playing = false;
        self.governor.buffer_progress().set_playback_active(false);
        (self.callback)(PlayerEvent::StateChange(
            PlaybackState::Paused,
            self.current_seq,
        ));
        self.resume_store.flush_if_due(true);
    }

    /// The page that loaded the retained track is gone. What `handle_stop` left to resume is
    /// no longer one a page can name, and the intent it recorded for a quality swap's re-assert
    /// belongs to that page too: the one that follows boots stopped, as a reload does.
    pub(super) fn handle_orphan(&mut self) {
        self.orphaned = true;
        self.resume_on_reassert = false;
    }

    pub(super) fn handle_stop(&mut self, event_seq: u32) {
        self.cancel_crossfade();
        // Reconciliation: an SDK stop is pause-retain, not teardown. The pipeline is kept so
        // a same-track re-assert (a quality swap's cancel-redispatch) resumes in place instead
        // of rebuilding. A genuine track change tears the previous track down through `load()`,
        // and position is preserved.
        self.current_seq = event_seq;
        // Capture is_playing before handle_pause clears it, then record it as the
        // re-assert resume intent (a stop is a re-assert candidate).
        let was_playing = self.is_playing;
        self.handle_pause();
        self.resume_on_reassert = was_playing;
        // handle_pause above arms the release only when something was playing; a
        // terminal stop (the SDK's reset(); nothing has to follow) must release
        // even from an already-paused session: re-arm without a was_playing gate.
        #[cfg(target_os = "windows")]
        if self.is_asio_mode && self.asio_handle.is_some() {
            self.asio_release_at = Some(std::time::Instant::now() + super::ASIO_IDLE_RELEASE);
        }
        // Surface the SDK/UI/Connect/SMTC-visible "stopped" state even though the
        // pipeline is retained internally (handle_pause emits Paused first; the
        // final state observers settle on is Stopped).
        self.idle_state = PlaybackState::Stopped;
        (self.callback)(PlayerEvent::StateChange(
            PlaybackState::Stopped,
            self.current_seq,
        ));
    }

    /// A seek persists its target only once a backend has accepted it. Persisting on entry
    /// left a refused seek in the store, and a later auto-resume honoured a position
    /// playback never reached. The deferred branches persist nothing: the load they wait on
    /// may never land.
    pub(super) fn handle_seek(&mut self, time: f64) {
        // A seek during a fade is ambiguous by construction: it would have to name
        // one of two live tracks. Cancel and snap to the outgoing one.
        self.cancel_crossfade();
        // Latest-seek-wins
        let mut latest_time = time;
        while let Ok(next_cmd) = self.cmd_rx.try_recv() {
            match next_cmd {
                PlayerCommand::Seek(t) => {
                    latest_time = t;
                }
                other => self.pending_cmds.push(other),
            }
        }

        // Every backend turns this into a `symphonia` Time, which rejects anything past i64
        // seconds and returns nothing at all on four of its five call sites. Bound it once
        // here rather than at each of them: an out-of-range target then reaches the reader
        // and comes back refused, with an answer, instead of vanishing.
        const MAX_SEEK_SECONDS: f64 = 86_400.0;
        let latest_time = if latest_time.is_finite() {
            latest_time.clamp(0.0, MAX_SEEK_SECONDS)
        } else {
            0.0
        };

        self.pending_resume_seek = None;

        #[cfg(target_os = "windows")]
        {
            if self.is_asio_mode {
                if self.has_track {
                    // Dispatch first: announcing before a successful send would pin the UI on
                    // a target the decoder never received. Seeks in place and signals the
                    // control thread to flush the ring (ResetForSeek).
                    let gen_id = self.next_seek_gen();
                    let dispatched = self
                        .asio_seek_tx
                        .as_ref()
                        .is_some_and(|tx| tx.send((latest_time, gen_id)).is_ok());
                    if dispatched {
                        let target = latest_time.max(0.0);
                        self.last_asio_pos = Some(target);
                        // Pin the UI to the seek target until the decoder's ResetForSeek
                        // rebases the position (the control thread reports stale until then).
                        // Cleared by the matching SeekSettled.
                        self.seeking = true;
                        self.seek_target = Some(target);
                        (self.callback)(PlayerEvent::StateChange(
                            PlaybackState::Seeking,
                            self.current_seq,
                        ));
                        (self.callback)(PlayerEvent::TimeUpdate(target, self.current_seq));
                    } else {
                        // No live decoder to seek: report where playback actually is instead
                        // of the position the user asked for and will not be given.
                        let live = self.last_asio_pos.unwrap_or(0.0);
                        (self.callback)(PlayerEvent::TimeUpdate(live, self.current_seq));
                        crate::vprintln!("[SEEK]   asio: no live decoder; position restored");
                    }
                } else {
                    // No live decoder yet: queue as a user seek override, which
                    // supersedes any auto-resume the upcoming load resolves. Untagged, the
                    // queue keeps nothing: the target the UI already took would stand.
                    if !self.queue_user_seek(latest_time) {
                        let live = self.last_asio_pos.unwrap_or(0.0);
                        (self.callback)(PlayerEvent::TimeUpdate(live, self.current_seq));
                    }
                }
                return;
            }
        }

        #[cfg(target_os = "windows")]
        {
            if self.is_exclusive_mode {
                if self.has_track {
                    // Dispatch first: announcing before a successful send pins the UI on a
                    // target that never converges and nothing clears. Seeks in place (no
                    // respawn or re-probe) and signals the render to flush.
                    let gen_id = self.next_seek_gen();
                    let dispatched = self
                        .exclusive_seek_tx
                        .as_ref()
                        .is_some_and(|tx| tx.send((latest_time, gen_id)).is_ok());
                    if dispatched {
                        let target = latest_time.max(0.0);
                        // Cover the window before the decoder's first post-seek
                        // TimeUpdate (a back-to-shared re-arm may read this).
                        self.last_exclusive_pos = Some(target);
                        // Pin the UI to the target until the render's position converges, as
                        // the ASIO branch does: until the flush lands the backend still
                        // reports the pre-seek position, which would walk the bar back.
                        self.seeking = true;
                        self.seek_target = Some(target);
                        self.seek_wall_start = Some(std::time::Instant::now());
                        (self.callback)(PlayerEvent::StateChange(
                            PlaybackState::Seeking,
                            self.current_seq,
                        ));
                        (self.callback)(PlayerEvent::TimeUpdate(target, self.current_seq));
                    } else {
                        // No live decoder to seek: report where playback actually is instead
                        // of the position the user asked for and will not be given.
                        let live = self.last_exclusive_pos.unwrap_or(0.0);
                        (self.callback)(PlayerEvent::TimeUpdate(live, self.current_seq));
                        crate::vprintln!("[SEEK]   exclusive: no live decoder; position restored");
                    }
                } else {
                    // No live decoder yet: queue as a user seek override, which
                    // supersedes any auto-resume the upcoming load resolves. Untagged, the
                    // queue keeps nothing: the target the UI already took would stand.
                    if !self.queue_user_seek(latest_time) {
                        let live = self.last_exclusive_pos.unwrap_or(0.0);
                        (self.callback)(PlayerEvent::TimeUpdate(live, self.current_seq));
                    }
                }
                return;
            }
        }

        if self.try_skip_pre_seek(latest_time) {
            // The pre-seek already put playback at the target, making it the position.
            if let Some(track_id) = self.current_track_id.as_ref() {
                self.resume_store.set(track_id, latest_time);
                self.resume_store.flush_if_due(false);
            }
            crate::vprintln!("[SEEK]   skipped (pre-seeked matches {:.2}s)", latest_time);
            return;
        }

        // Minted before the borrow below, as the two bypass branches above already do.
        let gen_id = self.next_seek_gen();
        if let Some(ref tx) = self.decode_cmd_tx {
            self.seeking = true;
            self.seek_target = Some(latest_time);
            self.seek_wall_start = Some(std::time::Instant::now());
            self.governor.buffer_progress().request_seek_preload_pause();
            if let Some(ref m) = self.cpal_muted {
                m.store(true, Relaxed);
            }
            (self.callback)(PlayerEvent::StateChange(
                PlaybackState::Seeking,
                self.current_seq,
            ));
            (self.callback)(PlayerEvent::TimeUpdate(
                latest_time.max(0.0),
                self.current_seq,
            ));

            let _ = tx.send(DecodeCommand::Seek(latest_time, gen_id));
            crate::vprintln!(
                "[SEEK]   sent: {:.2}s ({})",
                latest_time,
                if self.cached.is_some() {
                    "cached/RAM"
                } else {
                    "streaming"
                }
            );
        } else if self.has_track && self.loading_gen.is_none() {
            // The pipeline died (a fatal decode error nulls the sender); queuing is a lie
            // here, the next load overwrites the queue. Clear before setting: set() drops
            // anything at or under RESUME_MIN_SECONDS and would leave an older entry standing.
            let live = self.played_position_secs();
            if let Some(track_id) = self.current_track_id.clone() {
                self.resume_store.clear(&track_id);
                self.resume_store.set(&track_id, live);
                self.resume_store.flush_if_due(true);
            }
            (self.callback)(PlayerEvent::TimeUpdate(live, self.current_seq));
            crate::vprintln!("[SEEK]   no decoder and no load coming; position restored");
        } else {
            // The IPC handler flushed the target before dispatching. Staying silent leaves a
            // position playback never reached on screen until some later load announces one.
            // The intention still waits; a respawn skipping handle_load reads this slot.
            let live = self.played_position_secs();
            self.pending_resume_seek = Some(latest_time);
            (self.callback)(PlayerEvent::TimeUpdate(live, self.current_seq));
            crate::vprintln!("[SEEK]   queued until player ready; position restored");
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/player/thread/transport.rs"]
mod tests;
