use crate::player::{LoadRequest, Volume};
use crate::state::TrackInfo;

#[derive(Debug, serde::Serialize, Clone)]
pub struct AudioDevice {
    #[serde(rename = "controllableVolume")]
    pub controllable_volume: bool,
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Ready,
    Active,
    Paused,
    Stopped,
    Seeking,
    Idle,
    Completed,
}

impl PlaybackState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
            Self::Seeking => "seeking",
            Self::Idle => "idle",
            Self::Completed => "completed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// A variant marked `expect(dead_code)` is built only by the Windows backends.
pub enum DeviceErrorKind {
    NotFound,
    FormatNotSupported,
    #[cfg_attr(not(target_os = "windows"), expect(dead_code))]
    ExclusiveModeNotAllowed,
    #[cfg_attr(not(target_os = "windows"), expect(dead_code))]
    ExclusiveFormatUnsupported,
    #[cfg_attr(not(target_os = "windows"), expect(dead_code))]
    Locked,
    Disconnected,
    Unknown,
    #[cfg_attr(not(target_os = "windows"), expect(dead_code))]
    AsioDriverNotFound,
    #[cfg_attr(not(target_os = "windows"), expect(dead_code))]
    AsioFormatUnsupported,
    #[cfg_attr(not(target_os = "windows"), expect(dead_code))]
    AsioInitFailed,
    #[cfg_attr(not(target_os = "windows"), expect(dead_code))]
    AsioRateUnsupported,
    /// The backend thread itself stopped without saying so. Every typed failure below
    /// reports before exiting; a PANIC produces no error value at all, and no hook turns
    /// one into anything. Left unnoticed it stranded two threads: the render thread, and
    /// the decoder parked against a `consumed` counter only that thread advances.
    #[cfg_attr(not(target_os = "windows"), expect(dead_code))]
    ExclusiveBackendDied,
    #[cfg_attr(not(target_os = "windows"), expect(dead_code))]
    AsioBackendDied,
}

impl DeviceErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "devicenotfound",
            Self::FormatNotSupported => "deviceformatnotsupported",
            Self::ExclusiveModeNotAllowed => "deviceexclusivemodenotallowed",
            Self::ExclusiveFormatUnsupported => "deviceexclusiveformatunsupported",
            Self::Locked => "devicelocked",
            Self::Disconnected => "devicedisconnected",
            Self::Unknown => "deviceunknownerror",
            Self::AsioDriverNotFound => "deviceasiodrivernotfound",
            Self::AsioFormatUnsupported => "deviceasioformatunsupported",
            Self::AsioInitFailed => "deviceasioinitfailed",
            Self::AsioRateUnsupported => "deviceasiorateunsupported",
            Self::ExclusiveBackendDied => "deviceexclusivebackenddied",
            Self::AsioBackendDied => "deviceasiobackenddied",
        }
    }
}

/// `mediaerror` codes the SDK recognizes (its `mediaErrorCodeMap`); any other
/// string degrades to `errorCode: undefined` on the SDK side. Typed: a site
/// cannot invent an off-contract code. `file_checksum_mismatch` has no producer here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaErrorCode {
    /// NPO01 - the source couldn't be fetched (HTTP failure, missing segment).
    NoSuchFile,
    /// NPO03 - the source was fetched but can't be read (probe/decode failure).
    UnreadableFile,
}

impl MediaErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoSuchFile => "no_such_file",
            Self::UnreadableFile => "unreadable_file",
        }
    }
}

/// The audio output backend a `player.devices.set` selects. The three modes are
/// mutually exclusive (a radio choice), and a single enum replaces the former
/// `(exclusive, asio)` boolean pair, which could encode the invalid both-true
/// state. `Exclusive`/`Asio` are Windows-only at runtime (the frontend toggles
/// gating them are win32-only); other platforms only ever see `Shared`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Shared output through the OS mixer (cpal).
    Shared,
    /// Exclusive WASAPI: bypasses the OS mixer.
    Exclusive,
    /// ASIO driver output: bypasses the OS mixer.
    Asio,
}

/// Last `MediaFormat` emitted for the committed track (shared-path probe only;
/// ASIO/exclusive loads don't emit one). Re-emitted on a same-track re-assert
/// leaving the renderer's nulled per-load format snapshot filled.
#[derive(Debug, Clone, Copy)]
pub struct MediaFormatSnapshot {
    pub codec: &'static str,
    /// The rate stored in the file.
    pub sample_rate: u32,
    /// The rate the output stream is running at, which is what the listener hears. It
    /// differs from `sample_rate` whenever the decoder conformed the track, and the
    /// badge lied about that for as long as only the file's rate was reported.
    pub output_sample_rate: u32,
    pub bit_depth: Option<u32>,
    pub channels: u16,
    pub bytes: u64,
}

impl MediaFormatSnapshot {
    pub fn to_event(self) -> PlayerEvent {
        PlayerEvent::MediaFormat {
            codec: self.codec,
            sample_rate: self.sample_rate,
            output_sample_rate: self.output_sample_rate,
            bit_depth: self.bit_depth,
            channels: self.channels,
            bytes: self.bytes,
        }
    }
}

#[derive(Debug, Clone)]
pub enum PlayerEvent {
    TimeUpdate(f64, u32),
    /// Seconds, the load's event generation, and the id of the track the length was
    /// measured on. The id travels with the measurement because the only shared slot that
    /// names a track is rewritten by whichever load announces itself first, which is not
    /// the one being measured. `None` when the load carried no id, and a measurement that
    /// names no track is published under none.
    Duration(f64, u32, Option<String>),
    StateChange(PlaybackState, u32),
    /// A crossfade finished and the incoming track is now the current one. Means
    /// the same as `StateChange(Completed, seq)` to every consumer EXCEPT the
    /// auto-load, which must not start a track that is already playing.
    ///
    /// The load's event generation, and the id of the track promoted. The id travels for the
    /// same reason `Duration` carries one: a consumer that only learns "something was promoted"
    /// infers WHAT from its own bookkeeping, leaving the length measured in the same breath
    /// nothing to be judged against. `None` when the load carried no id.
    CrossfadePromoted(u32, Option<String>),
    AudioDevices(Vec<AudioDevice>, Option<String>),
    DeviceError(DeviceErrorKind),
    MediaFormat {
        codec: &'static str,
        sample_rate: u32,
        output_sample_rate: u32,
        bit_depth: Option<u32>,
        channels: u16,
        bytes: u64,
    },
    Version(&'static str),
    /// `seq` is the event generation of the load that failed, as every other per-load event
    /// carries one: a failure that does not name its load is charged to whichever load is
    /// current when it is read.
    ///
    /// `product_id` names the track that failed to load, when the failure is the load's own (the
    /// fetch or the probe, before the track played) and the load carried one. The renderer
    /// refuses a track it loaded itself on that name alone: the SDK does not know such a track.
    /// `None` for a failure of a track already playing, which the renderer leaves to the SDK.
    MediaError {
        error: String,
        code: MediaErrorCode,
        seq: u32,
        product_id: Option<String>,
    },
    /// Bytes stopped arriving for thirty seconds and the track cannot go on. Stops the
    /// player and raises TIDAL's own error banner; never advances the queue, and never
    /// touches the cache: the file was fine, the connection was not. Carries the load's event
    /// generation, as `MediaError` does.
    NetworkLost(u32),
    /// Re-arm a retained source into the shared pipeline: flush.rs reloads `track`
    /// at the generation snapshot (skipped if a newer load/stop superseded it),
    /// seeks to `position` (None = start), and auto-plays per `play`. Emitted on a
    /// no-pipeline play or when leaving exclusive mode.
    ReplayRequest {
        track: TrackInfo,
        expected_gen: u32,
        position: Option<f64>,
        play: bool,
    },
    #[cfg_attr(all(not(target_os = "windows"), not(test)), expect(dead_code))]
    VolumeSync(f64),
}

pub(super) enum PlayerCommand {
    Load {
        request: LoadRequest,
        auto_play: bool,
    },
    /// A load for `generation` has begun; the thread marks it in-flight.
    LoadStarted {
        generation: u32,
    },
    /// A load for `generation` ended (delivered/failed/dropped); the thread
    /// clears its in-flight marker and any play deferred on it.
    LoadSettled {
        generation: u32,
    },
    Play,
    /// A same-track re-assert from `Player::load`'s idempotent branch. Resumes
    /// if the track was playing when the pause-retain stop happened
    /// (`resume_on_reassert`) or if the load carried the user's play-intent
    /// (`want_play`); a user-paused re-assert with no intent stays paused.
    ReassertResume {
        want_play: bool,
        /// This load's own id for the committed track, the freshest there is. `None` leaves the
        /// id the thread already holds alone: a re-assert refreshes, it does not erase.
        product_id: Option<String>,
        /// The canonical track this re-assert was minted for, which the branch that mints it
        /// has already matched against the committed one. Carried because the match happened
        /// on the caller's thread and can be stale by the time this is handled.
        track_id: String,
    },
    Pause,
    Stop(u32),
    /// The page that loaded the track just stopped is gone (`StopScope::orphans`): until a load
    /// names a track again, the retained one is nobody's to resume.
    Orphan,
    Seek(f64),
    SetVolume(Volume),
    GetAudioDevices(Option<String>),
    SetAudioDevice {
        id: String,
        mode: OutputMode,
    },
    /// A load failed before reaching the pipeline: report the error, then settle
    /// the SDK's `load()` with `Duration(0.0, seq)` - its `mediaduration` await
    /// has no timeout: a mediaerror alone hangs it forever. Dropped when
    /// `load_gen` is superseded (a stale failure must not settle a newer load).
    LoadFailed {
        error: String,
        code: MediaErrorCode,
        seq: u32,
        load_gen: u32,
        /// The failed load's own id, never the thread's current one: this fires before
        /// `handle_load` ran, which leaves the thread still committed to the previous track.
        product_id: Option<String>,
    },
    #[cfg(target_os = "windows")]
    SetVolumeSync(bool),
    /// Crossfade overlap in seconds, zero for off. The switch and the slider
    /// collapse into one number here: every downstream check already reads zero
    /// as "no fade".
    SetCrossfadeSecs(u8),
}

#[cfg(test)]
#[path = "../../tests/unit/player/protocol.rs"]
mod tests;
