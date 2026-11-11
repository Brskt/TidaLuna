use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};

use crate::player::packet_failure::PacketFailure;

mod endpoint;
mod feeder;
mod render;

pub(super) use feeder::stream_flac_reader_to_wasapi;
use render::render_thread;

/// The stream a decoder opened, as the render adopts it: its identity, format and starting point.
pub(super) struct StreamStart {
    pub(super) stream_id: u32,
    /// The seq of the load whose decoder opened the stream, echoed on every transport state
    /// the render reports for it. Meaningful to the player alone, as `gen_id` is to a seek.
    pub(super) opened_under: u32,
    pub(super) sample_rate: u32,
    pub(super) channels: u32,
    pub(super) bits_per_sample: u32,
    pub(super) duration_secs: f64,
    pub(super) start_secs: f64,
    pub(super) start_paused: bool,
    pub(super) consumed: Arc<AtomicU64>,
}

pub(super) enum ExclusiveCommand {
    StartStream(StreamStart),
    PushPcm {
        stream_id: u32,
        pcm_data: Vec<u8>,
    },
    EndStream {
        stream_id: u32,
    },
    /// In-place seek applied by the live decoder: flush buffered PCM and
    /// re-base the reported position, without reopening the client.
    ResetForSeek {
        stream_id: u32,
        gen_id: u32,
        start_secs: f64,
    },
    /// The decoder could not seek. Carries no position: the render owns the answer and
    /// reports the seek unsettled, leaving the player to fall back on what it last knew.
    SeekFailed {
        stream_id: u32,
        gen_id: u32,
    },
    /// The decoder thread died mid-stream. It is the one exit that signals nothing on its
    /// own, which left the player holding a seek channel whose receiver was already gone.
    /// `cause` travels with it because the classification is only available here, where the
    /// error is still typed: the text alone cannot be re-read for it further down.
    DecodeFailed {
        stream_id: u32,
        error: String,
        cause: PacketFailure,
    },
    /// The decode thread died BEFORE adopting the stream, where `DecodeFailed` cannot reach:
    /// that variant only travels after this stream's own `StartStream`, and the idle arm drops
    /// it on that premise. Without this the failure left the transport at Ready, silent.
    StartupFailed {
        stream_id: u32,
        error: String,
        cause: PacketFailure,
    },
    /// Stream-scoped like PushPcm/EndStream (mirrors ASIO's Play/Pause): un-scoped, a
    /// premature Play (racing the decoder's probe) or a stale one from a superseded track
    /// resumed the OLD armed track's PCM until the new StartStream landed.
    Play {
        stream_id: u32,
    },
    Pause {
        stream_id: u32,
    },
    /// Drop the IAudioClient (free the endpoint for other apps) on a real stop,
    /// while keeping the render thread alive. The next StartStream reopens it.
    ReleaseDevice,
    Shutdown,
}

impl ExclusiveCommand {
    /// Names the handler a stall is charged to. The render loop drains every pending
    /// command inside a single iteration; the iteration alone never says which overran.
    fn label(&self) -> &'static str {
        match self {
            Self::StartStream(_) => "StartStream",
            Self::PushPcm { .. } => "PushPcm",
            Self::EndStream { .. } => "EndStream",
            Self::ResetForSeek { .. } => "ResetForSeek",
            Self::SeekFailed { .. } => "SeekFailed",
            Self::DecodeFailed { .. } => "DecodeFailed",
            Self::StartupFailed { .. } => "StartupFailed",
            Self::Play { .. } => "Play",
            Self::Pause { .. } => "Pause",
            Self::ReleaseDevice => "ReleaseDevice",
            Self::Shutdown => "Shutdown",
        }
    }
}

pub(super) enum ExclusiveEvent {
    TimeUpdate(f64),
    /// The answer to one dispatched seek, sent whatever the outcome. `position` is where
    /// playback actually is (the landing point when the decoder seeked, the untouched
    /// current position when it refused), never the target nobody reached. `refused`
    /// drives the resume store: a refused target must be evicted, not persisted. Distinct
    /// from `TimeUpdate`, sparing the player any guess about whether a periodic report is
    /// this seek's answer; `stream_id`-scoped like `Completed`, keeping an ack off the
    /// track that replaced it.
    SeekSettled {
        stream_id: u32,
        /// Echoed back from the command untouched. Judging an ack's freshness belongs to
        /// the player thread alone, the only party that knows which seek it awaits.
        gen_id: u32,
        position: f64,
        refused: bool,
    },
    /// A transport state of a stream, named by the load that opened it. The endpoint may report
    /// one after the player has moved on (a pause waits out the stop and reset of a dirty
    /// endpoint), and only that seq tells the player whose it is. Relayed whichever stream it
    /// names, never discarded: completion is what a superseded stream must not announce, and
    /// arrives apart.
    StateChange {
        opened_under: u32,
        state: super::PlaybackState,
    },
    /// Track finished (EOF + drained). Stream-scoped: a stale completion from a superseded
    /// stream must not clear a newer track, which would force a re-arm/double-load. Mirrors
    /// `AsioEvent::Completed`.
    Completed(u32),
    /// The adopted stream's track length. Stream-scoped like `SeekSettled`: a superseded
    /// stream reports one too, and downstream nothing can tell whose it was.
    Duration {
        stream_id: u32,
        secs: f64,
    },
    InitFailed(String),
    DeviceLocked(String),
    /// The device can't do THIS track's rate/bit-depth in exclusive (e.g. 88.2/176.4/192k on a
    /// 96k-max DAC), but exclusive itself works for other rates. Per-track shared fallback,
    /// keeping exclusive enabled; mirrors `AsioEvent::RateUnsupported`. Stream-scoped: a
    /// refusal costs seconds on a rate-locked device, long enough for the track it judges to
    /// stop being the one playing.
    FormatUnsupported {
        stream_id: u32,
    },
    /// The decoder thread died mid-stream. Settled like the shared path's fatal decode
    /// error, never like `Completed`: the track did not finish, and its resume point has
    /// to survive. Stream-scoped, keeping a superseded decoder's death off a newer track.
    DecodeFailed {
        stream_id: u32,
        error: String,
        cause: PacketFailure,
    },
    /// The decoder died before adopting the stream. Carries a cause like `DecodeFailed`: the
    /// probe reads the streaming buffer, which is the network path, so a stall at track start
    /// raises `TimedOut` as one mid-track does. Classified before `format!` erases the type.
    StartupFailed {
        stream_id: u32,
        error: String,
        cause: PacketFailure,
    },
}

pub(super) struct ExclusiveHandle {
    cmd_tx: mpsc::Sender<ExclusiveCommand>,
    event_rx: mpsc::Receiver<ExclusiveEvent>,
    thread: Option<JoinHandle<()>>,
}

impl ExclusiveHandle {
    /// Spawn the WASAPI render thread for the given device.
    /// `device_id` - wasapi device id string, or "default".
    pub fn spawn(device_id: String, gain: Arc<AtomicU32>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<ExclusiveCommand>();
        let (event_tx, event_rx) = mpsc::channel::<ExclusiveEvent>();

        let handle = thread::spawn(move || {
            render_thread(device_id, cmd_rx, event_tx, gain);
        });

        Self {
            cmd_tx,
            event_rx,
            thread: Some(handle),
        }
    }

    /// A handle wired to channels the caller owns, with no render thread behind it: a test can
    /// hand the player the very events the render would have sent. `thread: None` is what keeps
    /// `Drop` from joining one that was never spawned. Mirrors `AsioHandle::for_test`.
    #[cfg(test)]
    pub(crate) fn for_test() -> (
        Self,
        mpsc::Sender<ExclusiveEvent>,
        mpsc::Receiver<ExclusiveCommand>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::channel::<ExclusiveCommand>();
        let (event_tx, event_rx) = mpsc::channel::<ExclusiveEvent>();
        (
            Self {
                cmd_tx,
                event_rx,
                thread: None,
            },
            event_tx,
            cmd_rx,
        )
    }

    pub fn send(&self, cmd: ExclusiveCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// True once the render thread has stopped, whatever ended it. A FACT, not a timer:
    /// there is no false trip to guard against, which is what makes polling it safe where
    /// a deadline would not be. Reads the handle without taking it, leaving `shutdown` and
    /// `Drop` free to join as before.
    pub fn is_dead(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| t.is_finished())
    }

    pub fn command_sender(&self) -> mpsc::Sender<ExclusiveCommand> {
        self.cmd_tx.clone()
    }

    /// Drain all pending events (non-blocking).
    pub fn poll_events(&self) -> Vec<ExclusiveEvent> {
        let mut events = Vec::new();
        while let Ok(ev) = self.event_rx.try_recv() {
            events.push(ev);
        }
        events
    }

    pub fn shutdown(mut self) {
        let _ = self.cmd_tx.send(ExclusiveCommand::Shutdown);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for ExclusiveHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(ExclusiveCommand::Shutdown);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}
