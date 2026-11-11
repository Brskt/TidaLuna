use super::PlayerThread;
use super::load::resolve_start_position;
use crate::player::asio::host::AsioCommand;
use crate::player::packet_failure::PacketFailure;
use crate::player::wasapi::ExclusiveCommand;
use crate::player::{MediaErrorCode, PlaybackState, PlayerEvent, ResumePolicy};

mod asio;
mod exclusive;

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

/// Whether a backend's settlement answers the seek the player is actually waiting on.
/// `stream_id` alone only proves the decoder was not replaced; two seeks issued on one
/// live stream share it, leaving the older answer free to settle the newer. Pure, to be
/// unit-tested without a backend. Windows-only: only the bypass backends carry an ack.
#[cfg(target_os = "windows")]
fn seek_ack_is_current(
    current_stream_id: Option<u32>,
    current_gen: u32,
    ack_stream_id: u32,
    ack_gen: u32,
) -> bool {
    current_stream_id == Some(ack_stream_id) && current_gen == ack_gen
}

/// Whether a per-track device verdict may be charged to the track now playing. Two ids match
/// or nothing does: a refusal can take seconds, and a superseded stream's would otherwise mark
/// whatever track loaded while it negotiated. Pure, to be unit-tested without a backend.
#[cfg(target_os = "windows")]
fn verdict_names_current_stream(current: Option<u32>, verdict: Option<u32>) -> bool {
    matches!((current, verdict), (Some(a), Some(b)) if a == b)
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

    /// The release un-adopts the stream, so an in-flight seek's ack dies on the identity check
    /// and the pin outlives the track, which is why `stop_decode` clears this same trio when it
    /// retires a decoder. The target is re-emitted rather than re-read: the seek never
    /// converged, so `played_position_secs` still reports pre-seek.
    #[cfg(target_os = "windows")]
    fn settle_seek_on_release(&mut self) {
        if !self.seeking {
            return;
        }
        let position = self.effective_position();
        self.clear_seek_pin();
        (self.callback)(PlayerEvent::StateChange(
            self.settled_state(),
            self.current_seq,
        ));
        (self.callback)(PlayerEvent::TimeUpdate(position, self.current_seq));
    }

    /// A bypass decoder died mid-stream: the SDK is told why and left on a terminal state.
    /// The resume point is deliberately kept, since the listener never reached the end.
    ///
    /// `cause` decides only two answers: a dead connection holds the queue and keeps the cached
    /// bytes, an unusable source reports a media error and lets it advance. The rest (silencing
    /// the backend, arming its release, clearing the seek pin) is owed either way.
    #[cfg(target_os = "windows")]
    pub(super) fn settle_bypass_decode_failure(&mut self, error: String, cause: PacketFailure) {
        // Silence the backend and arm its release. Clearing bookkeeping is half of what
        // `stop_decode()` does for the shared path; it also drops the cpal stream. Here the
        // ring still holds DECODE_AHEAD_SECS of audio, and no EndStream means no completion.
        if self.is_exclusive_mode {
            if let Some(ref handle) = self.exclusive_handle {
                match self.current_exclusive_stream_id {
                    Some(stream_id) => handle.send(ExclusiveCommand::Pause { stream_id }),
                    None => crate::vprintln!("[WASAPI] decode failure: no live stream to silence"),
                }
            }
            // The debounced block checks for a handle itself: a timer set without one no-ops.
            self.exclusive_release_at =
                Some(std::time::Instant::now() + super::EXCLUSIVE_PAUSE_RELEASE);
        }
        if self.is_asio_mode {
            if let Some(ref handle) = self.asio_handle {
                match self.current_asio_stream_id {
                    Some(stream_id) => handle.send(AsioCommand::Pause { stream_id }),
                    None => crate::vprintln!("[ASIO]   decode failure: no live stream to silence"),
                }
            }
            self.asio_release_at = Some(std::time::Instant::now() + super::ASIO_IDLE_RELEASE);
        }
        // Cached bytes that will not decode must not stay indexed: every later play would fail
        // the same way, and the load path just refreshed their LRU position. Handed over rather
        // than done here, this being the control thread where a cache wipe holds the lock for
        // its whole duration. The ONLY retirement for a bypass decoder's failure: its
        // `DecodeFailed` never reaches the arm that drains the shared channel.
        // A stall is the case this must not touch: those bytes decoded fine until they
        // stopped arriving, and evicting them would cost a re-download of a good file.
        if cause == PacketFailure::Source
            && let Some(seen) = self.cached
            && let Some(tid) = self.current_track_id.clone()
        {
            crate::player::cache::AudioCache::drop_entry_detached(tid, seen);
        }
        match cause {
            // No `MediaError`: every code the SDK maps advances the queue, and a track whose
            // bytes stopped arriving has not earned that. The text is already logged by the
            // caller; what crosses to the SDK is the bare fact, as on the shared path.
            PacketFailure::Network => {
                crate::verr!("[BYPASS] Network lost, stopping the player");
                (self.callback)(PlayerEvent::NetworkLost(self.current_seq));
            }
            PacketFailure::Source => (self.callback)(PlayerEvent::MediaError {
                error,
                code: MediaErrorCode::UnreadableFile,
                seq: self.current_seq,
                product_id: None,
            }),
        }
        // The SDK's same-track recovery reload rebuilds instead of resuming a dead decoder.
        self.set_committed_track(None);
        self.has_track = false;
        self.is_playing = false;
        self.clear_seek_pin();
        self.exclusive_seek_tx = None;
        self.asio_seek_tx = None;
        self.current_track_id = None;
        self.idle_state = PlaybackState::Stopped;
        self.governor.buffer_progress().set_playback_active(false);
        (self.callback)(PlayerEvent::StateChange(
            PlaybackState::Stopped,
            self.current_seq,
        ));
    }

    /// Where playback actually is, asked of the backend that owns it. `played_samples` (the
    /// cpal counter) sits frozen at the last shared position while ASIO or exclusive is
    /// engaged, since neither bypass backend writes it. `None` before anything played.
    #[cfg(target_os = "windows")]
    pub(super) fn live_position_secs(&self) -> Option<f64> {
        if self.is_asio_mode {
            return self.last_asio_pos;
        }
        if self.is_exclusive_mode {
            return self.last_exclusive_pos;
        }
        let shared = self.effective_position();
        (shared > 0.0).then_some(shared)
    }

    /// Where a track leaving a bypass backend resumes: `live`, the backend's own floor-free
    /// position, else what `resume_store` holds for `retained`.
    #[cfg(target_os = "windows")]
    pub(super) fn resume_position(
        &self,
        live: Option<f64>,
        retained: Option<&crate::state::RetainedTrack>,
    ) -> Option<f64> {
        live.or_else(|| {
            retained.and_then(|r| {
                self.resume_store
                    .get(&crate::player::canonical_track_id(&r.track.url))
            })
        })
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/player/thread/bypass.rs"]
mod tests;
