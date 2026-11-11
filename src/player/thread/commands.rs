use super::PlayerThread;
use crate::player::PlayerEvent;

#[cfg(target_os = "windows")]
use super::load::resolve_start_position;
#[cfg(target_os = "windows")]
use crate::player::ResumePolicy;
#[cfg(target_os = "windows")]
use crate::player::asio::host::AsioHandle;
#[cfg(target_os = "windows")]
use crate::player::{ASIO_STREAM_SEQ, EXCLUSIVE_STREAM_SEQ, wasapi};
#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
#[cfg(target_os = "windows")]
use std::sync::{Arc, mpsc};
#[cfg(target_os = "windows")]
use std::thread;

/// Does a seek queued while no bypass decoder was live still apply to the track a load is
/// about to open? Its tag is the only thing that can answer. A restart answers no whatever
/// the tag holds: its contract is position 0.
#[cfg(target_os = "windows")]
pub(super) fn queued_seek_survives(
    queued_track: Option<&str>,
    track_id: &str,
    resume_policy: ResumePolicy,
) -> bool {
    match queued_track {
        Some(tagged) => tagged == track_id && !matches!(resume_policy, ResumePolicy::Restart),
        None => false,
    }
}

impl<F: Fn(PlayerEvent) + Send + 'static> PlayerThread<F> {
    /// Queue a seek issued while no bypass decoder is live, tagged with the track it
    /// targets, letting the upcoming load tell whether it still applies. Reports whether the
    /// seek was kept: a dropped one is owed an answer; nothing will make it come true.
    #[cfg(target_os = "windows")]
    pub(super) fn queue_user_seek(&mut self, time: f64) -> bool {
        match self.current_track_id.clone() {
            Some(track_id) => {
                self.user_seek_override = Some((track_id, time));
                true
            }
            // A decode failure drops the track identity before the SDK's recovery reload;
            // with nothing to tag, the seek has no load it could survive to.
            None => {
                crate::vprintln!("[SEEK]   queued seek dropped: no track to tag it with");
                false
            }
        }
    }

    /// No tag re-check here: `handle_load` is the only place that discards one for another.
    #[cfg(target_os = "windows")]
    pub(super) fn take_user_seek_override(&mut self) -> Option<f64> {
        self.user_seek_override.take().map(|(_, time)| time)
    }

    /// The position a bypass stream must open at. Both candidates compete for one slot, and
    /// a loser left armed reaches the next reader of that slot as a fresh seek.
    #[cfg(target_os = "windows")]
    pub(super) fn take_start_position(&mut self) -> Option<f64> {
        let queued = self.take_user_seek_override();
        let auto_resume = self.pending_resume_seek.take();
        resolve_start_position(queued, auto_resume)
    }

    /// Spawn the exclusive decoder for `buffer`, seeking the source to `seek_to`.
    /// A fresh `stream_id` makes the render drop the prior decoder's stale
    /// PushPcm. No `Stop` (it emits `Stopped`, which clears the host-side track);
    /// the new `StartStream` is the reset.
    #[cfg(target_os = "windows")]
    pub(super) fn spawn_exclusive_decoder(
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
    pub(super) fn start_exclusive_playback(
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

    /// Spawn the ASIO decoder for `buffer`, seeking the source to `seek_to`. Mirrors
    /// `spawn_exclusive_decoder`: a fresh `stream_id` makes the control thread drop
    /// the prior decoder's stale `PushPcm`; the new `StartStream` is the reset.
    #[cfg(target_os = "windows")]
    pub(super) fn spawn_asio_decoder(
        &mut self,
        buffer: crate::player::buffer::RamBuffer,
        seek_to: Option<f64>,
        start_paused: bool,
    ) {
        let Some(cmd_tx) = self.asio_handle.as_ref().map(|h| h.command_sender()) else {
            return;
        };
        if let Some(prev) = self.asio_stream_cancel.take() {
            prev.store(true, Relaxed);
            // Wake the retired reader to quiesce instead of parking on the read
            // timeout while the new stream contends for the same buffer.
            if let Some(ref buf) = self.current_buffer {
                buf.wake_readers();
            }
        }
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
    pub(super) fn start_asio_playback(
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
}

#[cfg(test)]
#[path = "../../../tests/unit/player/thread/commands.rs"]
mod tests;
