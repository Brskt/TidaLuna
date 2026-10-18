pub(crate) mod asio;
pub(crate) mod buffer;
pub(crate) mod cache;
pub(crate) mod crossfade;
pub(crate) mod dash;
mod declick;
pub(crate) mod ipc;
// Compiled where it serves and where it is tested: only the two Windows engines consult it,
// but the classification is pure and its tests earn their place on every host.
#[cfg(any(target_os = "windows", test))]
mod packet_failure;
mod resume;
mod thread;
#[cfg(target_os = "windows")]
mod throttle;
#[cfg(target_os = "windows")]
pub(crate) mod wasapi;

use crate::audio::bandwidth::GovernorHandle;
use crate::audio::preload;
use crate::state::{AUDIO_CACHE, CURRENT_METADATA, CURRENT_TRACK, HTTP_CLIENT_PLAYBACK, TrackInfo};
use buffer::{DownloadOwner, HeadStatus, RamBuffer};
use futures_util::stream::{self, StreamExt};
use std::sync::atomic::{
    AtomicU32, AtomicU64,
    Ordering::{AcqRel, Acquire, Relaxed},
};
use std::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The current load's generation and the origin that minted it, in ONE word.
///
/// Generation in the high 32 bits, origin in the low 8: `pack_xfade_done`'s layout one module
/// over, for its reason. Held in two atomics they were two separate writes, and two minters
/// racing left the newest generation beside the other one's origin, so `next_is_current` read a
/// Connect generation next to a Local origin and waved a renderer-staged track into a Connect
/// session. Durably, too, nothing rewriting the pair until the next mint: one race cost the rest
/// of the track.
///
/// Private on purpose, so the rule is never restated at a call site: a reader that wants the
/// generation asks [`current_gen`], one that weighs both asks [`current_load`], and neither can
/// sample half of a mint.
static LOAD_STATE: AtomicU64 = AtomicU64::new(0);
static EVENT_SEQ: AtomicU32 = AtomicU32::new(0);

/// Who issued the load that owns the current generation.
///
/// The generation says WHEN a load happened and never WHO asked for it: loads of both origins
/// mint from the same counter. The staged "next" track answers to the renderer's own queue and
/// to nothing else: promoted while another origin's load is current, it starts a track that
/// queue chose and this one never did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LoadOrigin {
    /// This host's own surfaces: the renderer's player delegate over the `player.*` IPC
    /// channels, the completion-time advance driven on its behalf, and a plugin. All of them
    /// answer to the renderer's queue. A re-arm and a stop from the OS controls are neither
    /// origin: they leave the player with whoever owned it (`continue_load`, [`StopScope`]).
    Local,
    /// The TIDAL Connect receiver, driving this speaker for a controller.
    Connect,
}

/// Whose music a stop halts, and in whose name it leaves the player.
///
/// A stop mints a generation like a load, and with it who owns the player afterwards. Minted
/// `Local` during a cast, it hands the player to the renderer, whose next load then replaces
/// the controller's track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopScope {
    /// In this origin's name, over whatever plays: the renderer's delegate (which the cast gate
    /// lets through only while this host owns the player) and the controller's own stop.
    As(LoadOrigin),
    /// Whatever plays, left with its owner: a Stop from the OS media controls or the tray, and
    /// quitting. A transport command, as the controller's own is: it changes nobody's claim.
    Playing,
    /// This host's own music only: a page's session or document ending (logout, the login
    /// transitions). A cast plays whether or not a page is loaded here, and a controller's
    /// session runs on the controller's credentials, not this host's login.
    OwnMusic,
    /// A cast's music only, handing the player back to this host: the controller's session is
    /// over. A session that never took the player leaves local music playing.
    Cast,
}

impl StopScope {
    /// The origin the stop mints in, given who owns the player, or `None` to leave it alone.
    pub(crate) fn claim(self, owner: LoadOrigin) -> Option<LoadOrigin> {
        match self {
            StopScope::As(origin) => Some(origin),
            StopScope::Playing => Some(owner),
            StopScope::OwnMusic => (owner == LoadOrigin::Local).then_some(LoadOrigin::Local),
            StopScope::Cast => (owner == LoadOrigin::Connect).then_some(LoadOrigin::Local),
        }
    }
}

/// What a load that carries an earlier one on did (see [`continue_load`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Continuation {
    /// Its load was still current: the new one is minted in the same owner's name.
    Started,
    /// A newer load or a stop minted since, and nothing was started over it.
    Superseded,
}

/// Split a `LOAD_STATE` word into its generation and origin. An origin byte that is not
/// `Connect` reads as `Local`: the initial value of an untouched process is this host's own.
#[inline]
fn unpack_load_state(word: u64) -> (u32, LoadOrigin) {
    let origin = if word & 0xFF == LoadOrigin::Connect as u64 {
        LoadOrigin::Connect
    } else {
        LoadOrigin::Local
    };
    ((word >> 32) as u32, origin)
}

/// Inverse of [`unpack_load_state`].
#[inline]
fn pack_load_state(cur_gen: u32, origin: LoadOrigin) -> u64 {
    ((cur_gen as u64) << 32) | (origin as u64)
}

/// Mint the next generation if `claim` accepts the load that is current, in the origin it
/// names. `None` when it declines, and the word is left as it was.
///
/// The decision, the generation and its origin land in one read-modify-write: `ui::flush` mints
/// from a spawned task holding no lock while the Connect receiver mints inside the `AppState`
/// lock, and a decision taken on a word read beforehand could act on a load already replaced.
fn mint_if(claim: impl Fn(u32, LoadOrigin) -> Option<LoadOrigin>) -> Option<u32> {
    LOAD_STATE
        .try_update(AcqRel, Acquire, |word| {
            let (cur_gen, owner) = unpack_load_state(word);
            claim(cur_gen, owner).map(|origin| pack_load_state(cur_gen.wrapping_add(1), origin))
        })
        .ok()
        .map(|previous| unpack_load_state(previous).0.wrapping_add(1))
}

/// Mint the next load generation, naming who it belongs to.
pub(crate) fn begin_load(origin: LoadOrigin) -> u32 {
    mint_if(|_, _| Some(origin)).expect("a claim that accepts every load always mints")
}

/// Mint the generation that carries `expected_gen`'s load on, in its owner's name, or `None`
/// once another load or a stop has minted since.
///
/// For a minter acting on a load it saw earlier, holding no lock (a re-arm, the completion-time
/// advance). The check shares the mint's exchange, or a load would start over a stop or a session
/// release landing in between.
pub(crate) fn continue_load(expected_gen: u32) -> Option<u32> {
    mint_if(|cur_gen, owner| (cur_gen == expected_gen).then_some(owner))
}

/// The current load's generation, for the readers that only ask "is this still my load?".
pub(crate) fn current_gen() -> u32 {
    unpack_load_state(LOAD_STATE.load(Acquire)).0
}

/// The current load's generation and origin, read as one.
///
/// `next_is_current` is the only reader that weighs both, and taking them one at a time is
/// precisely what let a staged track pass for fresh under someone else's session.
pub(crate) fn current_load() -> (u32, LoadOrigin) {
    unpack_load_state(LOAD_STATE.load(Acquire))
}

/// Mint the next event generation, for a load or a stop and nothing else. It is taken on the
/// requester's thread, in the order the requests were made; the player thread only ever installs
/// a seq one of them minted, so nothing it emits can outrank a command still in flight.
pub(crate) fn next_event_seq() -> u32 {
    EVENT_SEQ.fetch_add(1, Relaxed) + 1
}
#[cfg(target_os = "windows")]
static EXCLUSIVE_STREAM_SEQ: AtomicU32 = AtomicU32::new(0);
#[cfg(target_os = "windows")]
static ASIO_STREAM_SEQ: AtomicU32 = AtomicU32::new(0);

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

/// A volume percentage, always finite and within `0.0..=100.0`. Same reasoning as
/// [`OutputMode`] below: the invariant lives in the type, not in whichever function happens
/// to be its only consumer today.
///
/// Not cosmetic: `player.volume` takes any JSON number and the frontend forwards Redux's value
/// unclamped, so an input whose `/ 100.0` quotient passes `f32::MAX` casts to `f32::INFINITY`,
/// which the cpal callback multiplies into every sample. A non-finite input sanitizes to
/// silence, never to full scale, the wrong guess here having to be the quiet one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Volume(f64);

impl Volume {
    pub fn from_percent(v: f64) -> Self {
        Self(if v.is_finite() {
            v.clamp(0.0, 100.0)
        } else {
            0.0
        })
    }

    pub fn as_percent(self) -> f64 {
        self.0
    }

    /// The multiplier the output applies, within `0.0..=1.0`.
    pub fn as_gain(self) -> f32 {
        (self.0 / 100.0) as f32
    }

    fn is_silent(self) -> bool {
        self.0 <= 0.0
    }
}

/// The level the output plays at, and the one an unmute gives back.
///
/// Muted is a level of zero and nothing else: a Connect `setMute` carries no level, and an
/// unmute restores the last audible one, whoever set it. No mute flag can go stale behind it.
///
/// One level for the install, whichever TIDAL account a page loads: once anyone chose it
/// (`chosen`), the page takes it instead of TIDAL's per-account restore. A page level decided
/// before the page took this one in is refused ([`Self::admits`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputLevel {
    volume: Volume,
    audible: Volume,
    /// Bumped by every change, whoever made it, and by every page catching up with the level.
    seq: u64,
    /// `seq` of the last change no page made (a Connect controller, the OS mixer, an earlier
    /// launch) or of the last page catching up, 0 before either. A page's own change leaves it
    /// alone, or that page's next level, sent with an older report, would be refused.
    floor: u64,
    /// Someone set this level: an earlier launch, a controller, the OS mixer, a plugin or a page.
    /// Until then the output plays the thread's full scale, nobody's choice, and TIDAL's saved
    /// level is restored and seeds it.
    chosen: bool,
}

/// Who changed the level: a page of ours, or something no page sees happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LevelWriter {
    Page,
    Remote,
}

/// What Rust reports of its level: to TIDAL's store when a controller or the OS mixer moved it,
/// and to a page that loads, which cannot have seen those moves.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LevelReport {
    volume: f64,
    unmute_volume: f64,
    mute: bool,
    /// Someone chose this level: a loading page takes it, and TIDAL restores nothing over it.
    chosen: bool,
    seq: u64,
}

impl OutputLevel {
    const START: Self = Self {
        volume: Volume(100.0),
        audible: Volume(100.0),
        seq: 0,
        floor: 0,
        chosen: false,
    };

    /// The level an earlier launch left, as [`LevelStore`] saved it. Stamped as a change no page of
    /// this launch has seen: a loading page takes it over TIDAL's, and a page level decided before
    /// taking it in is refused, as after a controller's move.
    fn restored(volume: Volume, audible: Volume) -> Self {
        let mut level = Self::START;
        if !audible.is_silent() {
            level.audible = audible;
        }
        level.set(volume, LevelWriter::Remote);
        level
    }

    pub fn volume(self) -> Volume {
        self.volume
    }

    pub fn report(self) -> LevelReport {
        LevelReport {
            volume: self.volume.as_percent(),
            unmute_volume: self.audible.as_percent(),
            mute: self.volume.is_silent(),
            chosen: self.chosen,
            seq: self.seq,
        }
    }

    /// Whether a page's level may apply. `seen` is the newest report the page had taken in when
    /// it decided; one decided before the floor would undo a level that page never saw. A sender
    /// that states nothing (a plugin) is taken at its word.
    fn admits(self, seen: Option<u64>) -> bool {
        seen.is_none_or(|seen| seen >= self.floor)
    }

    /// A page that loads takes this level in. Every level decided before, by that page or any
    /// other, was decided without it; the page's own levels from here on carry this count.
    fn catch_up(&mut self) {
        self.seq += 1;
        self.floor = self.seq;
    }

    /// A page's level. One the output already plays changes nothing and is nobody's choice:
    /// TIDAL's native player applies the store's initial full scale when it is built, which must
    /// not stop TIDAL's saved level from seeding a level nobody chose yet. Returns whether the
    /// level changed.
    fn set_from_page(&mut self, volume: Volume) -> bool {
        if volume == self.volume {
            return false;
        }
        self.set(volume, LevelWriter::Page);
        true
    }

    fn stamp(&mut self, by: LevelWriter) {
        self.seq += 1;
        self.chosen = true;
        if by == LevelWriter::Remote {
            self.floor = self.seq;
        }
    }

    fn set(&mut self, volume: Volume, by: LevelWriter) {
        self.volume = volume;
        if !volume.is_silent() {
            self.audible = volume;
        }
        self.stamp(by);
    }

    /// Only a Connect controller mutes: TIDAL's own mute reaches Rust as a level of zero.
    fn mute(&mut self) {
        self.volume = Volume(0.0);
        self.stamp(LevelWriter::Remote);
    }

    /// A no-op on the level while audible: it is then the last audible one already.
    fn unmute(&mut self) {
        self.volume = self.audible;
        self.stamp(LevelWriter::Remote);
    }
}

/// Keeps [`OutputLevel`] across launches in `settings.db`, whoever set it. Written on every
/// change: a killed launch never reaches the quit path's flush. A write still queued takes the
/// newest level, not one write per step of a dragged slider or phone.
struct LevelStore {
    db: crate::db::DbActor,
    /// The newest level no queued write has taken yet: `Some` exactly while one write is queued.
    unsaved: std::sync::Arc<std::sync::Mutex<Option<OutputLevel>>>,
}

impl LevelStore {
    /// Called under the `OutputLevel` lock, so levels arrive here in the order they were set.
    fn save(&self, level: OutputLevel) {
        if self
            .unsaved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(level)
            .is_some()
        {
            return;
        }
        let unsaved = std::sync::Arc::clone(&self.unsaved);
        self.db.post(move |_, settings| {
            let level = unsaved.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(level) = level {
                crate::settings::save_output_level(
                    settings,
                    level.volume.as_percent(),
                    level.audible.as_percent(),
                );
            }
        });
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
    MediaError {
        error: String,
        code: MediaErrorCode,
        seq: u32,
    },
    MaxConnectionsReached,
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

#[derive(Debug, Clone, Copy, PartialEq)]
enum ResumePolicy {
    Disabled,
    /// Start at 0 and discard a seek queued against this track while its device was
    /// released. Distinct from `Disabled`, which a re-arm also sends when it merely
    /// has no position to offer: only this variant expresses the SDK's load contract.
    Restart,
    Auto,
    Explicit(f64),
}

struct LoadRequest {
    buffer: RamBuffer,
    load_gen: u32,
    seq: u32,
    track_id: String,
    /// The frontend's own id for this track, distinct from `track_id`: that one is the
    /// canonical URL, which the frontend never sees and cannot match a measurement against.
    product_id: Option<String>,
    resume_policy: ResumePolicy,
    load_start: std::time::Instant,
    cached: bool,
    format: String,
}

enum LoadStep {
    Handled,
    Miss,
}

struct LoadContext {
    load_gen: u32,
    event_seq: u32,
    load_start: std::time::Instant,
    resume_policy: ResumePolicy,
    auto_play: bool,
    cmd_tx: mpsc::Sender<PlayerCommand>,
    format: String,
    product_id: Option<String>,
    /// Cancelled when a newer load supersedes this one or on stop.
    cancel_token: CancellationToken,
    /// The player's governor, for the download this load opens.
    governor: GovernorHandle,
}

impl LoadContext {
    fn is_stale(&self) -> bool {
        current_gen() != self.load_gen
    }

    fn publish_load(&self, buffer: RamBuffer, cached: bool, track_id: String) {
        let _ = self.cmd_tx.send(PlayerCommand::Load {
            request: LoadRequest {
                buffer,
                load_gen: self.load_gen,
                seq: self.event_seq,
                track_id,
                product_id: self.product_id.clone(),
                resume_policy: self.resume_policy,
                load_start: self.load_start,
                cached,
                format: self.format.clone(),
            },
            auto_play: self.auto_play,
        });
    }

    fn fail_load(&self, error: String, code: MediaErrorCode) {
        let _ = self.cmd_tx.send(PlayerCommand::LoadFailed {
            error,
            code,
            seq: self.event_seq,
            load_gen: self.load_gen,
            product_id: self.product_id.clone(),
        });
    }
}

/// Sends `LoadSettled{generation}` on Drop: the thread's in-flight marker is
/// cleared on every task exit (completion, return, panic, cancellation) - a
/// panicked/aborted load can't leave a play deferring forever. Gen-matched on
/// the thread side; a stale settle is a no-op.
struct LoadSettleGuard {
    cmd_tx: mpsc::Sender<PlayerCommand>,
    generation: u32,
}

impl Drop for LoadSettleGuard {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(PlayerCommand::LoadSettled {
            generation: self.generation,
        });
    }
}

enum PlayerCommand {
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
    EmitMaxConnections,
    #[cfg(target_os = "windows")]
    SetVolumeSync(bool),
    /// Crossfade overlap in seconds, zero for off. The switch and the slider
    /// collapse into one number here: every downstream check already reads zero
    /// as "no fade".
    SetCrossfadeSecs(u8),
}

pub struct Player {
    cmd_tx: mpsc::Sender<PlayerCommand>,
    rt_handle: tokio::runtime::Handle,
    load_handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Current download's cancellation token; cancelled on a new load or stop.
    download_cancel: std::sync::Mutex<Option<CancellationToken>>,
    /// The track currently committed in the pipeline, as `(canonical_id, format)`,
    /// or `None` when nothing is loaded. The player thread sets it in `handle_load`
    /// (commit time, alongside `current_track_id`) and clears it on every track-end
    /// path; `load` reads it on the IPC thread to resume a same-track re-assert
    /// instead of rebuilding. It tracks the *committed* track, not the *requested*
    /// one; a duplicate load racing a still-in-flight load of a different track
    /// cannot falsely match and resume a stale pipeline. stop() is pause-retain, and
    /// this survives a stop and the following play() resumes it in place.
    committed_track: std::sync::Arc<std::sync::Mutex<Option<(String, String)>>>,
    /// Every level the output plays, recorded in the order it was set. An unmute restores from
    /// here, because this handle is the one place all writers pass through: the frontend's
    /// `player.volume`, a Connect receiver, and the OS mixer's reports.
    output_level: std::sync::Mutex<OutputLevel>,
    /// `None` without a settings database, as in unit tests: the level then lasts one launch.
    level_store: Option<LevelStore>,
    /// The one governor arbitrating this player's link between its playback and its preload.
    governor: GovernorHandle,
}

pub(crate) use crate::util::fmt::{format_bytes, format_ms, short_id};

// Gated: its only caller outside `player` is Windows-only, and clippy denies an unused re-export.
#[cfg(target_os = "windows")]
pub(crate) use thread::output::is_default_selector;

fn http_version_str(v: reqwest::Version) -> &'static str {
    if v == reqwest::Version::HTTP_3 {
        "3"
    } else if v == reqwest::Version::HTTP_2 {
        "2"
    } else if v == reqwest::Version::HTTP_11 {
        "1.1"
    } else if v == reqwest::Version::HTTP_10 {
        "1.0"
    } else {
        "?"
    }
}

/// Extract the CDN cache status from the response headers (x-cache).
pub(crate) fn cdn_cache_status(resp: &reqwest::Response) -> &str {
    resp.headers()
        .get("x-cache")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("?")
}

pub(crate) fn log_response_headers(resp: &reqwest::Response, prefix: &str) {
    use std::fmt::Write as _;
    let h = resp.headers();
    let ver = http_version_str(resp.version());
    let ct = h
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    let mut info = format!("HTTP/{ver} | type={ct}");
    for &(header, label) in &[
        ("content-encoding", "enc"),
        ("server", "server"),
        ("x-cache", "CDN"),
        ("x-cache-hits", "hits"),
        ("age", "age"),
        ("via", "via"),
    ] {
        if let Some(val) = h.get(header).and_then(|v| v.to_str().ok()) {
            let _ = write!(info, " | {label}={val}");
        }
    }
    crate::vprintln!("{prefix} {info}");
}

pub(crate) fn canonical_track_id(url: &str) -> String {
    url.split('?').next().unwrap_or(url).to_string()
}

/// Refreshes the retained credential only if the retained track is still the one
/// `canonical_url_id` names. Not redundant with the caller's guard, which reads
/// `committed_track`, set once a load reaches the pipeline, where this record is written
/// synchronously: the two legitimately disagree mid-load, and a duplicate `load(A)` racing
/// behind `load(B)` would stamp A's credential onto B's still-running download, killing it
/// silently at its next reconnect. A write is authorised by the state it writes, never by
/// another that lags it. `load_gen` is left alone: re-signing is not a new load.
pub(crate) fn refresh_retained_credential(
    retained: Option<&mut crate::state::RetainedTrack>,
    canonical_url_id: &str,
    url: &str,
    key: &str,
    product_id: Option<&str>,
) {
    let Some(track) = retained
        .map(|retained| &mut retained.track)
        .filter(|track| canonical_track_id(&track.url) == canonical_url_id)
    else {
        return;
    };
    track.url = url.to_string();
    track.key = key.to_string();
    // Refreshes what this load knows and erases nothing it does not: a load arriving without an
    // id must not blank the one a later replay of this same source still needs.
    if let Some(id) = product_id {
        track.product_id = Some(id.to_string());
    }
}

/// The id retained for a source, when the retained source is this one. A recover is handed a
/// fresh credential for the track already playing and no identity at all; the retained copy is
/// where that identity was last known.
fn retained_product_id(canonical_url_id: &str) -> Option<String> {
    CURRENT_TRACK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retained()
        .map(|retained| &retained.track)
        .filter(|track| canonical_track_id(&track.url) == canonical_url_id)
        .and_then(|track| track.product_id.clone())
}

/// The url a running download must fetch with, for the track it started for. The signed url is a
/// credential, not identity: the SDK re-signs it every load and re-resolves it once its 1h expiry
/// lapses, so a captured copy goes stale while the task legitimately runs on, and reconnects and
/// range restarts read through here instead. `None` means the task's track was replaced and it
/// must stop, making explicit an invariant `load_with_policy` already keeps.
pub(crate) fn refreshed_fetch_url(
    task_canonical_id: &str,
    retained: Option<&crate::state::TrackInfo>,
) -> Option<String> {
    retained
        .filter(|track| canonical_track_id(&track.url) == task_canonical_id)
        .map(|track| track.url.clone())
}

/// True when a `load` targets the already-committed track: same canonical id
/// (query stripped, since each load re-signs the CDN URL) and format. Re-loading
/// would rebuild an identical pipeline: the caller resumes instead. Both ids
/// are canonical (caller strips the URL; `committed.0` is set at commit time).
fn is_same_active_track(
    committed: Option<&(String, String)>,
    canonical_url_id: &str,
    format: &str,
) -> bool {
    committed.is_some_and(|(id, fmt)| fmt == format && id == canonical_url_id)
}

/// Whether the announced metadata describes a track other than this load's. The slot answers
/// "what was announced last", never "which track is this load", and a Rust-driven advance runs
/// ahead of the announcement. Only a POSITIVE disagreement counts: a load carrying no id of its
/// own (what a recover through `retained_product_id` looks like) is no evidence of staleness,
/// and treating it as such would withhold a title that was correct.
pub(crate) fn announcement_is_stale(load: Option<&str>, announced: Option<&str>) -> bool {
    matches!((load, announced), (Some(a), Some(b)) if a != b)
}

/// `product_id` is the load's own id, used only to tell whether the announced metadata
/// describes this load. Logging-only: nothing downstream reads the banner.
fn print_track_banner(format: &str, product_id: Option<&str>) {
    let Ok(lock) = CURRENT_METADATA.lock() else {
        crate::vprintln!("[PLAYER] CURRENT_METADATA lock poisoned, skipping banner");
        return;
    };
    let format_upper = format.to_uppercase();
    let announced_id = lock.as_ref().and_then(|m| m.id.as_deref());
    let unannounced = product_id.filter(|load| announcement_is_stale(Some(load), announced_id));
    let (title, artist, quality) = match (unannounced, lock.as_ref()) {
        (Some(id), _) => (id, "not announced yet", format_upper.as_str()),
        (None, Some(m)) => (
            if m.title.is_empty() {
                "Unknown"
            } else {
                m.title.as_str()
            },
            if m.artist.is_empty() {
                "Unknown"
            } else {
                m.artist.as_str()
            },
            if m.quality.is_empty() {
                format_upper.as_str()
            } else {
                m.quality.as_str()
            },
        ),
        (None, None) => ("Unknown", "Unknown", format_upper.as_str()),
    };
    crate::vprintln!("══════════════════════════════════════════");
    crate::vprintln!("  {} - {}", title, artist);
    crate::vprintln!("  Quality: {} | Format: {}", quality, format);
    crate::vprintln!("══════════════════════════════════════════");
}

/// Does this look like a media container we could decode? Rejects a cache entry
/// whose key no longer fits it. Format-agnostic on purpose: the `format` string
/// comes from TIDAL's SDK and its vocabulary is not ours to enumerate.
fn looks_like_media(data: &[u8]) -> bool {
    if data.len() < 12 {
        return false;
    }
    data.starts_with(b"fLaC")
        || &data[4..8] == b"ftyp" // ISO BMFF: mp4 / m4a
        || data.starts_with(b"OggS")
        || data.starts_with(b"ID3")
        || looks_like_mpeg_frame(data)
}

/// Does `data` open on an ADTS-AAC frame header? Layer 00 with the 12th sync bit is a value MPEG
/// audio reserves; the two families cannot be confused. The frame's own length field must point at
/// a successor sync, which is what separates this from a bare sync word: 11 bits accept one random
/// byte pair in 2048.
fn looks_like_adts_frame(data: &[u8]) -> bool {
    if data.len() < 7 || data[0] != 0xFF {
        return false;
    }
    if data[1] & 0xF0 != 0xF0 || data[1] & 0x06 != 0 {
        return false;
    }
    // 13..15 are reserved rates; channel config 0 defers to an AudioSpecificConfig an ADTS
    // stream never carries.
    let sampling = (data[2] >> 2) & 0x0F;
    let channels = ((data[2] & 0x01) << 2) | (data[3] >> 6);
    if sampling > 12 || channels == 0 {
        return false;
    }
    // A frame cannot be shorter than the header it starts with.
    let frame_len =
        (((data[3] & 0x03) as usize) << 11) | ((data[4] as usize) << 3) | ((data[5] >> 5) as usize);
    if frame_len < 7 {
        return false;
    }
    match data.get(frame_len..frame_len + 2) {
        Some(next) => next[0] == 0xFF && next[1] & 0xF0 == 0xF0,
        // The successor lies past what we were given; the header alone stands.
        None => true,
    }
}

/// A cache-side plausibility check, not a security gate. It is built to reject accidental noise, not
/// an adversarial sequence, which is why `plugin.download` validates container structure instead. Raw
/// ADTS reaches here since symphonia's `aac` build probes with no hint; the MPEG-audio half stops at
/// the header because finding the next frame needs the bitrate tables.
fn looks_like_mpeg_frame(data: &[u8]) -> bool {
    if looks_like_adts_frame(data) {
        return true;
    }
    // Everything below reads the first two bytes, and the ADTS shape is already excluded by the
    // call above: layer 00 with the 12th sync bit is a value MPEG audio reserves.
    if data.len() < 7 || data[0] != 0xFF || (data[1] & 0xF0 == 0xF0 && data[1] & 0x06 == 0) {
        return false;
    }

    if data[1] & 0xE0 != 0xE0 {
        return false;
    }
    // Version 01, layer 00 and sampling 11 are reserved; bitrate 1111 is invalid and
    // 0000 is the free format nothing here emits. Finding the next frame would need
    // the bitrate tables; this stops at the header - what slips through is caught
    // by the decoder, which retires the entry.
    let version = (data[1] >> 3) & 0x03;
    let layer = (data[1] >> 1) & 0x03;
    let bitrate = (data[2] >> 4) & 0x0F;
    let sampling = (data[2] >> 2) & 0x03;
    version != 0x01 && layer != 0x00 && bitrate != 0x00 && bitrate != 0x0F && sampling != 0x03
}

/// Why a cache entry could not be turned into playable bytes. The distinction is
/// load-bearing: only `Corrupt` condemns the stored file.
enum CacheReadError {
    /// The file is gone from disk: the index row is an orphan.
    Orphaned,
    /// Could not read the file, or could not use the key. Neither says anything about
    /// the stored ciphertext, and the entry must survive: evicting would discard a good
    /// file the next attempt would have to download again.
    Unreadable(String),
    /// Decrypted, but not into a container we recognise: the stored ciphertext no
    /// longer matches this key.
    Corrupt(String),
}

/// Read a cache entry and hand back playable bytes. Blocking and CPU-bound (a whole
/// file read plus an AES pass over every byte): callers run it off the runtime.
fn read_cache_entry(path: &std::path::Path, key: &str) -> Result<Vec<u8>, CacheReadError> {
    let mut data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CacheReadError::Orphaned);
        }
        Err(e) => return Err(CacheReadError::Unreadable(format!("read failed: {e}"))),
    };

    // Key-side only: `new` unwraps the key ID and `decrypt_in_place` can fail solely
    // while building the cipher from it; neither outcome depends on the disk bytes.
    if !key.is_empty()
        && let Err(e) = crate::audio::decrypt::FlacDecryptor::new(key)
            .and_then(|d| d.decrypt_in_place(&mut data, 0))
    {
        return Err(CacheReadError::Unreadable(format!("unusable key: {e}")));
    }

    // AES-CTR is unauthenticated: a stale key decrypts to noise and still returns
    // Ok. This must run before `touch`, which would refresh the LRU position and keep
    // a bad entry from ever evicting.
    if !looks_like_media(&data) {
        return Err(CacheReadError::Corrupt(
            "not a known container after decrypt".to_string(),
        ));
    }

    Ok(data)
}

/// `key` is the track's wrapped content key: cache files hold TIDAL's
/// ciphertext; a hit is only playable once decrypted. An empty key means the
/// stream was never encrypted, matching the download path's own check.
async fn try_cache_hit(ctx: &LoadContext, track_id: &str, key: &str) -> LoadStep {
    let cache_t0 = std::time::Instant::now();

    // Short lock: index lookup only (no disk I/O)
    let path = {
        let Ok(cache) = AUDIO_CACHE.lock() else {
            crate::vprintln!("[CACHE]  Lock poisoned, skipping cache lookup");
            return LoadStep::Miss;
        };
        cache.lookup_path(track_id)
    };

    let Some(path) = path else {
        let cache_ms = cache_t0.elapsed().as_secs_f64() * 1000.0;
        crate::vprintln!(
            "[CACHE]  Miss ({}) | lookup: {}",
            short_id(track_id, 40),
            format_ms(cache_ms)
        );
        return LoadStep::Miss;
    };

    // Gate before the read, not after: skipping through cached tracks would otherwise
    // decrypt each one in full only to throw the result away.
    if ctx.is_stale() {
        crate::vprintln!("[LOAD #{}] stale before cache read, dropping", ctx.load_gen);
        return LoadStep::Handled;
    }

    let read = {
        let path = path.clone();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || read_cache_entry(&path, &key)).await
    };

    let data = match read {
        Ok(Ok(data)) => data,
        Ok(Err(CacheReadError::Orphaned)) => {
            if let Ok(mut cache) = AUDIO_CACHE.lock() {
                cache.remove_index_entry(track_id);
            }
            return LoadStep::Miss;
        }
        Ok(Err(CacheReadError::Unreadable(reason))) => {
            crate::vprintln!(
                "[CACHE]  Keeping entry {}: {reason}",
                short_id(track_id, 40)
            );
            return LoadStep::Miss;
        }
        Ok(Err(CacheReadError::Corrupt(reason))) => {
            if let Ok(mut cache) = AUDIO_CACHE.lock() {
                match cache.drop_entry(track_id) {
                    cache::DropOutcome::Dropped | cache::DropOutcome::NoRow => crate::vprintln!(
                        "[CACHE]  Dropped unusable entry {}: {reason}",
                        short_id(track_id, 40)
                    ),
                    // The row outlives the failed delete, leaving `lookup_path` to hand back
                    // the same bytes on every later read. Ungated because the only retries (an
                    // eviction pass, the next store of this id) are not guaranteed to come.
                    cache::DropOutcome::FileKept => crate::verr!(
                        "[CACHE]  Unusable entry {} will keep failing, its file would not go: {reason}",
                        short_id(track_id, 40)
                    ),
                }
            }
            return LoadStep::Miss;
        }
        Err(e) => {
            crate::vprintln!("[CACHE]  Read task failed: {e}");
            return LoadStep::Miss;
        }
    };

    // Short lock: update access metadata
    if let Ok(cache) = AUDIO_CACHE.lock() {
        cache.touch(track_id);
    }

    let cache_ms = cache_t0.elapsed().as_secs_f64() * 1000.0;
    if ctx.is_stale() {
        return LoadStep::Handled;
    }
    crate::vprintln!(
        "[CACHE]  Hit: {} | {} | read: {} | total: {}",
        short_id(track_id, 40),
        format_bytes(data.len() as u64),
        format_ms(cache_ms),
        format_ms(ctx.load_start.elapsed().as_secs_f64() * 1000.0)
    );
    let buffer = RamBuffer::from_complete(data);
    ctx.publish_load(buffer, true, track_id.to_string());
    LoadStep::Handled
}

async fn try_preload_hit(ctx: &LoadContext, track: &TrackInfo, track_id: &str) -> LoadStep {
    let preload_t0 = std::time::Instant::now();
    if let Some(preloaded) = preload::take_preloaded_if_match(track).await {
        let preload_ms = preload_t0.elapsed().as_secs_f64() * 1000.0;
        if ctx.is_stale() {
            crate::vprintln!(
                "[LOAD #{}] stale after preload check, dropping",
                ctx.load_gen
            );
            // The take spent the record and adopted this download onto the playback queue;
            // nothing else names it any more. Dropped without this it runs to the end of the
            // file, competing for bandwidth with the track the listener actually skipped to.
            preloaded.buffer.cancel_download();
            return LoadStep::Handled;
        }
        // Adopted is not the same as usable: the slot is filled the instant the download
        // starts, and these bytes may still be in flight. The player thread probes this buffer
        // with a blocking read: the wait belongs here, in this task, and not there.
        if !preload::head_has_landed(&preloaded.buffer, || !ctx.is_stale()).await {
            // Spent and unusable: the record is gone, and nothing else will stop this
            // download. A stale load wanted nothing anyway; otherwise the ordinary path
            // below opens the copy that will actually be listened to.
            preloaded.buffer.cancel_download();
            if ctx.is_stale() {
                return LoadStep::Handled;
            }
            crate::vprintln!("[PRELOAD] Adopted copy never became playable, loading it afresh");
            return LoadStep::Miss;
        }
        crate::vprintln!(
            "[PRELOAD] Hit: {} | check: {} | total: {}",
            format_bytes(preloaded.buffer.total_len()),
            format_ms(preload_ms),
            format_ms(ctx.load_start.elapsed().as_secs_f64() * 1000.0)
        );
        let buffer = preloaded.buffer;
        ctx.publish_load(buffer, false, track_id.to_string());
        return LoadStep::Handled;
    }

    let preload_ms = preload_t0.elapsed().as_secs_f64() * 1000.0;
    crate::vprintln!("[PRELOAD] Miss | check: {}", format_ms(preload_ms));
    LoadStep::Miss
}

async fn start_stream_load(ctx: &LoadContext, url: &str, key: &str, track_id: &str) {
    let load_gen = ctx.load_gen;

    if ctx.is_stale() {
        crate::vprintln!("[LOAD #{load_gen}] stale before HTTP, dropping");
        return;
    }

    let req_start = std::time::Instant::now();
    let resp = match HTTP_CLIENT_PLAYBACK.get(url).send().await {
        Ok(r) => {
            if !r.status().is_success() {
                let status = r.status();
                crate::vprintln!("[ERROR]  Upstream status: {}", status);
                if status.as_u16() == 429 {
                    let _ = ctx.cmd_tx.send(PlayerCommand::EmitMaxConnections);
                } else {
                    ctx.fail_load(format!("HTTP {}", status), MediaErrorCode::NoSuchFile);
                }
                return;
            }
            r
        }
        Err(e) => {
            let text = crate::util::network_error_text(e);
            crate::vprintln!("[ERROR]  Request failed: {text}");
            ctx.fail_load(
                format!("request failed: {text}"),
                MediaErrorCode::NoSuchFile,
            );
            return;
        }
    };
    let ttfb_ms = req_start.elapsed().as_secs_f64() * 1000.0;
    let cdn = cdn_cache_status(&resp);
    crate::vprintln!(
        "[NET]    TTFB: {} | CDN: {} | HTTP/{}",
        format_ms(ttfb_ms),
        cdn,
        http_version_str(resp.version())
    );
    log_response_headers(&resp, "[NET]   ");

    if ctx.is_stale() {
        crate::vprintln!("[LOAD #{load_gen}] stale after HTTP TTFB, dropping");
        return;
    }

    let total_len = resp.content_length().unwrap_or(0);
    if total_len == 0 {
        crate::vprintln!(
            "[FETCH]  No Content-Length, full download... (TTFB: {})",
            format_ms(ttfb_ms)
        );
        // Handed the response already open, not left to race a second one against it: a
        // body nobody reads keeps an HTTP/1.1 connection held and unusable until it drops,
        // and this one would not drop until the whole fallback download had finished.
        match preload::fetch_and_decrypt(url, key, resp, &ctx.governor).await {
            Ok(fetched) => {
                if ctx.is_stale() {
                    crate::vprintln!("[LOAD #{load_gen}] stale after full download, dropping");
                    return;
                }
                crate::vprintln!(
                    "[FETCH]  Done ({} in {:.0}ms)",
                    format_bytes(fetched.data.len() as u64),
                    ctx.load_start.elapsed().as_secs_f64() * 1000.0
                );
                let buffer =
                    RamBuffer::from_complete_with_ciphertext(fetched.data, fetched.ciphertext);
                ctx.publish_load(buffer, false, track_id.to_string());
            }
            Err(e) => {
                crate::vprintln!("[ERROR]  Fetch failed: {}", e);
                ctx.fail_load(format!("fetch failed: {e}"), MediaErrorCode::NoSuchFile);
            }
        }
        return;
    }

    crate::vprintln!(
        "[NET]    Size: {} (load #{load_gen})",
        format_bytes(total_len)
    );

    // The load's own token becomes the buffer's, rather than a second one beside it: the
    // next load reaches this download through the player's slot, a buffer swap reaches it
    // through the buffer, and both arrive at the same token.
    let (buffer, writer) = RamBuffer::new(
        total_len,
        DownloadOwner::Playback,
        ctx.cancel_token.clone(),
        ctx.governor.clone(),
    );

    preload::start_download(resp, url.to_string(), key.to_string(), writer);

    // Pre-buffer 64KB before handing to decoder
    const PRE_BUFFER_TARGET: u64 = 64 * 1024;
    const PRE_BUFFER_TIMEOUT_MS: u64 = 2000;

    let prebuf_start = std::time::Instant::now();
    let prebuf_deadline = prebuf_start + std::time::Duration::from_millis(PRE_BUFFER_TIMEOUT_MS);
    loop {
        if ctx.is_stale() {
            crate::vprintln!("[LOAD #{load_gen}] stale during pre-buffer, dropping");
            buffer.cancel();
            return;
        }
        match buffer.head_status(PRE_BUFFER_TARGET) {
            HeadStatus::Landed => break,
            // Published all the same, and that is deliberate: the buffer carries the failure,
            // `RamBuffer::read` hands it to the decode thread on the first read before any
            // range check, and the listener gets a media error out of it. Refusing to publish
            // would swallow the only report of the failure there is. Only the waiting was
            // ever wrong here: only the waiting ends.
            HeadStatus::Ended(_) => {
                crate::vprintln!(
                    "[LOAD #{load_gen}] download ended at {}KB/{}KB, publishing what landed",
                    buffer.written() / 1024,
                    PRE_BUFFER_TARGET / 1024
                );
                break;
            }
            HeadStatus::Filling => {}
        }
        let remaining = prebuf_deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            crate::vprintln!(
                "[LOAD #{load_gen}] pre-buffer timeout ({}KB/{}KB in {}ms)",
                buffer.written() / 1024,
                PRE_BUFFER_TARGET / 1024,
                PRE_BUFFER_TIMEOUT_MS
            );
            break;
        }
        let _ = tokio::time::timeout(remaining, buffer.notified()).await;
    }

    crate::vprintln!(
        "[LOAD #{load_gen}] pre-buffered {}KB in {:.0}ms, sending Load",
        buffer.written() / 1024,
        prebuf_start.elapsed().as_secs_f64() * 1000.0
    );
    ctx.publish_load(buffer, false, track_id.to_string());
}

impl Player {
    pub fn new<F>(callback: F, rt_handle: tokio::runtime::Handle) -> Self
    where
        F: Fn(PlayerEvent) + Send + 'static,
    {
        // `DB.get()` rather than `db()`, whose accessor panics when no actor was installed, which
        // is every unit test that builds a player.
        Self::with_settings(callback, rt_handle, crate::state::DB.get().cloned())
    }

    pub(crate) fn with_settings<F>(
        callback: F,
        rt_handle: tokio::runtime::Handle,
        settings: Option<crate::db::DbActor>,
    ) -> Self
    where
        F: Fn(PlayerEvent) + Send + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel::<PlayerCommand>();

        // The fallback is not invented: `load_volume_sync` answers `true` for a settings row that
        // does not exist yet, so a missing actor and a fresh install agree. Same trade
        // `PlayerThread::new` makes for `crossfade_secs`.
        #[cfg(target_os = "windows")]
        let volume_sync_enabled = settings
            .as_ref()
            .is_none_or(|db| db.call_settings(crate::settings::load_volume_sync));
        #[cfg(not(target_os = "windows"))]
        let volume_sync_enabled = true;

        let restored = settings
            .as_ref()
            .and_then(|db| db.call_settings(crate::settings::load_output_level))
            .map(|(volume, audible)| {
                OutputLevel::restored(Volume::from_percent(volume), Volume::from_percent(audible))
            });

        let committed_track = std::sync::Arc::new(std::sync::Mutex::new(None::<(String, String)>));
        let thread_committed = committed_track.clone();
        let thread_level = restored.map(OutputLevel::volume);
        // On the runtime this player spawns its loads on, where the governor's task runs.
        let governor = {
            let _entered = rt_handle.enter();
            crate::audio::bandwidth::spawn_governor()
        };
        let thread_governor = governor.clone();
        std::thread::spawn(move || {
            if let Some(mut pt) = thread::PlayerThread::new(
                cmd_rx,
                callback,
                volume_sync_enabled,
                thread_level,
                thread_committed,
                thread_governor,
            ) {
                pt.run();
            }
        });

        Self {
            cmd_tx,
            rt_handle,
            load_handle: std::sync::Mutex::new(None),
            download_cancel: std::sync::Mutex::new(None),
            committed_track,
            output_level: std::sync::Mutex::new(restored.unwrap_or(OutputLevel::START)),
            level_store: settings.map(|db| LevelStore {
                db,
                unsaved: std::sync::Arc::new(std::sync::Mutex::new(None)),
            }),
            governor,
        }
    }

    /// The governor a preload staged for this player shares with its playback.
    pub fn governor(&self) -> &GovernorHandle {
        &self.governor
    }

    /// `load_gen` is minted by the caller, which is where it is decided whose load this is and
    /// whether it may start at all. Answers the event generation every event of this load
    /// carries.
    fn load_with_policy(
        &self,
        url: String,
        format: String,
        key: String,
        product_id: Option<String>,
        resume_policy: ResumePolicy,
        auto_play: bool,
        load_gen: u32,
    ) -> anyhow::Result<u32> {
        let _ = self.cmd_tx.send(PlayerCommand::LoadStarted {
            generation: load_gen,
        });
        let event_seq = next_event_seq();
        crate::vprintln!("[LOAD #{load_gen}] start");
        print_track_banner(&format, product_id.as_deref());

        if let Some(prev) = self
            .load_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            prev.abort();
        }

        // Cancel the previous track's download; a skip must not leak it streaming a
        // full file into RAM (start_download races this token).
        let cancel_token = CancellationToken::new();
        if let Some(prev) = self
            .download_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(cancel_token.clone())
        {
            prev.cancel();
        }

        // Reset governor buffer progress: stale counters from the previous track
        // would otherwise throttle the new download.
        self.governor.reset_buffer_progress();

        {
            let mut lock = CURRENT_TRACK.lock().unwrap_or_else(|e| e.into_inner());
            // Stamped with the generation the caller minted, well before this write: a reader
            // sampling the generation apart could see it while the slot still held the previous
            // track.
            lock.publish_load(
                load_gen,
                Some(TrackInfo {
                    url: url.clone(),
                    format: format.clone(),
                    key: key.clone(),
                    product_id: product_id.clone(),
                }),
            );
        }

        let ctx = LoadContext {
            load_gen,
            event_seq,
            load_start: std::time::Instant::now(),
            resume_policy,
            auto_play,
            cmd_tx: self.cmd_tx.clone(),
            format: format.clone(),
            product_id,
            cancel_token,
            governor: self.governor.clone(),
        };

        let handle = self.rt_handle.spawn(async move {
            let _settle = LoadSettleGuard {
                cmd_tx: ctx.cmd_tx.clone(),
                generation: ctx.load_gen,
            };
            let track = TrackInfo {
                url: url.clone(),
                format: format.clone(),
                key: key.clone(),
                product_id: ctx.product_id.clone(),
            };
            let track_id = canonical_track_id(&url);

            // The track being loaded stops being the "next" one here, whichever of
            // the three branches below serves it. Only the preload-hit branch used
            // to clear the staged record. A cache hit (which returns first, and
            // is what a preload deliberately skips fetching for) left it naming the
            // track that is now playing.
            crate::audio::preload::clear_next_track_if_match(&track_id).await;

            if let LoadStep::Handled = try_cache_hit(&ctx, &track_id, &key).await {
                return;
            }
            if let LoadStep::Handled = try_preload_hit(&ctx, &track, &track_id).await {
                return;
            }
            start_stream_load(&ctx, &url, &key, &track_id).await;
        });

        *self.load_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);

        Ok(event_seq)
    }

    pub fn load(
        &self,
        url: String,
        format: String,
        key: String,
        product_id: Option<String>,
        restart: bool,
        want_play: bool,
    ) -> anyhow::Result<()> {
        // Idempotent reconcile: when the SDK re-loads the track currently COMMITTED in
        // the pipeline, either restart it (a fresh play instance) or resume it in place
        // (a keep-position re-assert), without a full rebuild. The comparison is against
        // the committed track (set by the player thread in handle_load), never the
        // requested track: a duplicate load racing a still-in-flight load of a
        // different track cannot falsely match and touch a stale pipeline. stop() is
        // pause-retain, and the committed track survives. A genuine track change
        // (different canonical id/format) falls through to a full load.
        let canonical_url_id = canonical_track_id(&url);
        let same_committed = {
            let committed = self
                .committed_track
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            is_same_active_track(committed.as_ref(), &canonical_url_id, &format)
        };
        if same_committed {
            if restart {
                // Fresh play instance (TIDAL minted a new referenceId, threaded here as
                // `restart`): per the SDK's load contract (load = start at 0), restart from 0
                // via a fresh cached load (cache hit, no re-download), auto-playing only if
                // want_play. Reporting position 0 also stops Redux's position-reconcile seek
                // burst that a kept position would otherwise provoke.
                crate::vprintln!(
                    "[LOAD]   same track, fresh play instance -> restart at 0 (want_play={}): {}",
                    want_play,
                    short_id(&canonical_url_id, 60)
                );
                return self
                    .load_with_policy(
                        url,
                        format,
                        key,
                        product_id,
                        ResumePolicy::Restart,
                        want_play,
                        begin_load(LoadOrigin::Local),
                    )
                    .map(|_| ());
            }
            // Takes the new credential, keeps the play instance. The skip below correctly answers
            // "same track?" but must not also mean "still fetchable": every same-track load carries a
            // freshly signed url (HW-measured), and keeping the stale one meant the next reconnect
            // used a signature the CDN had dropped, 403ing the track dead.
            //
            // A refresh, not a rebuild: a quality-swap re-assert still keeps its position; only the
            // credential changes, and `refreshed_fetch_url` is what makes the download read it.
            {
                let mut retained = crate::state::CURRENT_TRACK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                refresh_retained_credential(
                    retained.retained_mut(),
                    &canonical_url_id,
                    &url,
                    &key,
                    product_id.as_deref(),
                );
            }
            // Same-track re-assert keeping the play instance (a quality-swap re-load):
            // resumes if PLAYING pre-stop (resume_on_reassert), or if want_play is set --
            // click-to-play on a restored paused track is stop+load(same) with NO
            // player.play (SDK-verified): want_play is the only resume signal then.
            crate::vprintln!(
                "[LOAD]   idempotent reload skipped (same committed track, want_play={}): {}",
                want_play,
                short_id(&canonical_url_id, 60)
            );
            // The id travels with the re-assert: this branch never reaches `handle_load`, which
            // is the only other place the thread learns which track it is measuring. Dropping
            // it here left a gapless-advanced track nameless for the rest of its life.
            let _ = self.send_cmd(PlayerCommand::ReassertResume {
                want_play,
                product_id,
                track_id: canonical_url_id,
            });
            return Ok(());
        }
        // Different track (genuine select / queue advance): fresh load. `want_play` folds
        // the SELECT's play-intent into the load: no separate player.play arrives while
        // the OLD track is still committed (decide_play would Resume it -> audible bleed).
        self.load_with_policy(
            url,
            format,
            key,
            product_id,
            ResumePolicy::Disabled,
            want_play,
            begin_load(LoadOrigin::Local),
        )
        .map(|_| ())
    }

    /// A load over whatever plays, in `origin`'s name: the Connect receiver's, on a controller's
    /// behalf. The renderer's completion-time advance carries its own load on instead
    /// ([`Player::advance`]). Answers the event generation every event of the load carries.
    pub fn load_and_play(
        &self,
        url: String,
        format: String,
        key: String,
        product_id: Option<String>,
        origin: LoadOrigin,
    ) -> anyhow::Result<u32> {
        self.load_with_policy(
            url,
            format,
            key,
            product_id,
            ResumePolicy::Disabled,
            true,
            begin_load(origin),
        )
    }

    /// The completion-time advance: play the track staged as next, if the load it was staged
    /// under is still the current one. Taking the record proved it (`next_is_current`), but this
    /// runs in a spawned task holding no lock, and a controller's load can take the player between
    /// the take and this mint.
    pub fn advance(&self, next: crate::state::RetainedTrack) -> anyhow::Result<Continuation> {
        let Some(load_gen) = continue_load(next.load_gen) else {
            return Ok(Continuation::Superseded);
        };
        let track = next.track;
        self.load_with_policy(
            track.url,
            track.format,
            track.key,
            track.product_id,
            ResumePolicy::Disabled,
            true,
            load_gen,
        )
        .map(|_| Continuation::Started)
    }

    /// Load a DASH stream by fetching init + media segments and concatenating them. Answers the
    /// event generation every event of the load carries.
    pub fn load_dash(
        &self,
        init_url: String,
        segment_urls: Vec<String>,
        format: String,
        product_id: Option<String>,
        origin: LoadOrigin,
    ) -> anyhow::Result<u32> {
        // The shared choke point. The IPC channel validates its own arguments, but the
        // Connect receiver reaches this function directly in Rust and handed it an empty
        // list, which loads as a header-only fMP4 and "completes" in zero samples. Which of
        // the two called is also why `origin` is a parameter and not something read here:
        // this function cannot tell them apart, and the staged-next guard has to.
        if segment_urls.is_empty() {
            anyhow::bail!("load_dash: refusing an empty segment list");
        }

        let load_gen = begin_load(origin);
        let _ = self.cmd_tx.send(PlayerCommand::LoadStarted {
            generation: load_gen,
        });
        let event_seq = next_event_seq();
        crate::vprintln!(
            "[DASH-LOAD #{load_gen}] start - {} segments",
            segment_urls.len()
        );
        print_track_banner(&format, product_id.as_deref());

        if let Some(prev) = self
            .load_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            prev.abort();
        }
        self.governor.reset_buffer_progress();

        // DASH isn't replayable via load_and_play (segmented); clear the
        // retained source: a post-DASH play re-arms nothing, not a stale track.
        CURRENT_TRACK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publish_load(load_gen, None);

        let cmd_tx = self.cmd_tx.clone();
        let handle = self.rt_handle.spawn(async move {
            let _settle = LoadSettleGuard {
                cmd_tx: cmd_tx.clone(),
                generation: load_gen,
            };
            let load_start = std::time::Instant::now();
            let is_stale = || current_gen() != load_gen;

            crate::vprintln!("[DASH-LOAD #{load_gen}] fetching init segment...");
            let init_data = match HTTP_CLIENT_PLAYBACK.get(&init_url).send().await {
                Ok(r) if r.status().is_success() => match r.bytes().await {
                    Ok(b) => b.to_vec(),
                    Err(e) => {
                        let text = crate::util::network_error_text(e);
                        crate::vprintln!("[ERROR]  DASH init segment read failed: {text}");
                        let _ = cmd_tx.send(PlayerCommand::LoadFailed {
                            error: format!("DASH init segment: {text}"),
                            code: MediaErrorCode::NoSuchFile,
                            seq: event_seq,
                            load_gen,
                            product_id: product_id.clone(),
                        });
                        return;
                    }
                },
                Ok(r) => {
                    crate::vprintln!("[ERROR]  DASH init segment HTTP {}", r.status());
                    let _ = cmd_tx.send(PlayerCommand::LoadFailed {
                        error: format!("DASH init HTTP {}", r.status()),
                        code: MediaErrorCode::NoSuchFile,
                        seq: event_seq,
                        load_gen,
                        product_id: product_id.clone(),
                    });
                    return;
                }
                Err(e) => {
                    let text = crate::util::network_error_text(e);
                    crate::vprintln!("[ERROR]  DASH init segment request failed: {text}");
                    let _ = cmd_tx.send(PlayerCommand::LoadFailed {
                        error: format!("DASH init request: {text}"),
                        code: MediaErrorCode::NoSuchFile,
                        seq: event_seq,
                        load_gen,
                        product_id: product_id.clone(),
                    });
                    return;
                }
            };

            if is_stale() {
                crate::vprintln!("[DASH-LOAD #{load_gen}] stale after init, dropping");
                return;
            }

            crate::vprintln!(
                "[DASH-LOAD #{load_gen}] init: {} | {:.0}ms",
                format_bytes(init_data.len() as u64),
                load_start.elapsed().as_secs_f64() * 1000.0
            );

            let segment_count = segment_urls.len();

            // Fetch segments with bounded concurrency, appending each to the
            // output buffer as it arrives and dropping it immediately. This caps
            // both peak RAM (no transient second copy of the whole file) and the
            // number of simultaneous CDN connections, unlike a join_all that
            // fires every segment request at once. `buffered` preserves order.
            const DASH_MAX_CONCURRENT: usize = 6;

            let mut mp4_data = init_data;
            let mut segments = stream::iter(segment_urls.into_iter().enumerate())
                .map(|(i, url)| async move {
                    match HTTP_CLIENT_PLAYBACK.get(&url).send().await {
                        Ok(r) if r.status().is_success() => r.bytes().await.map_err(|e| {
                            format!(
                                "DASH segment {i} read: {}",
                                crate::util::network_error_text(e)
                            )
                        }),
                        Ok(r) => Err(format!("DASH segment {i} HTTP {}", r.status())),
                        Err(e) => Err(format!(
                            "DASH segment {i} request: {}",
                            crate::util::network_error_text(e)
                        )),
                    }
                })
                .buffered(DASH_MAX_CONCURRENT);

            while let Some(result) = segments.next().await {
                if is_stale() {
                    crate::vprintln!("[DASH-LOAD #{load_gen}] stale mid-fetch, dropping");
                    return;
                }
                match result {
                    Ok(data) => mp4_data.extend_from_slice(&data),
                    Err(msg) => {
                        crate::vprintln!("[ERROR]  {msg}");
                        let _ = cmd_tx.send(PlayerCommand::LoadFailed {
                            error: msg,
                            code: MediaErrorCode::NoSuchFile,
                            seq: event_seq,
                            load_gen,
                            product_id: product_id.clone(),
                        });
                        return;
                    }
                }
            }

            let total_ms = load_start.elapsed().as_secs_f64() * 1000.0;
            crate::vprintln!(
                "[DASH-LOAD #{load_gen}] done: {} segments, {} total in {:.0}ms",
                segment_count,
                format_bytes(mp4_data.len() as u64),
                total_ms
            );

            let buffer = RamBuffer::from_complete(mp4_data);
            let track_id = format!("dash-{load_gen}");
            let _ = cmd_tx.send(PlayerCommand::Load {
                request: LoadRequest {
                    buffer,
                    load_gen,
                    seq: event_seq,
                    track_id,
                    product_id,
                    resume_policy: ResumePolicy::Disabled,
                    load_start,
                    cached: false,
                    format,
                },
                auto_play: true,
            });
        });

        *self.load_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        Ok(event_seq)
    }

    /// An explicit target seeks there (when past the resume floor); otherwise
    /// fall back to the stored resume position for the track.
    fn resume_policy_for(target_time: Option<f64>) -> ResumePolicy {
        match target_time {
            Some(t) if t.is_finite() && t > resume::RESUME_MIN_SECONDS => ResumePolicy::Explicit(t),
            _ => ResumePolicy::Auto,
        }
    }

    pub fn recover(
        &self,
        url: String,
        format: String,
        key: String,
        target_time: Option<f64>,
    ) -> anyhow::Result<()> {
        // The SDK's `recover` delegate carries only a url and a key: the id comes from the
        // retained source instead. A recover re-fetches the track already playing, never
        // another; the retained id is that track's own.
        let product_id = retained_product_id(&canonical_track_id(&url));
        self.load_with_policy(
            url,
            format,
            key,
            product_id,
            Self::resume_policy_for(target_time),
            false,
            begin_load(LoadOrigin::Local),
        )
        .map(|_| ())
    }

    /// Re-arm the shared pipeline from a `ReplayRequest`: `Some(position)` seeks
    /// there (past the resume floor; below it starts from the beginning), `None`
    /// restarts; `play` auto-plays. Single entry point for the ReplayRequest
    /// consumer, keeping the dispatch out of flush.rs. `expected_gen` is the load being
    /// re-armed: the same track again, in its owner's name, and only while nothing has minted
    /// since. A re-arm over a newer load would abort it, and over a stop would undo it.
    pub fn rearm(
        &self,
        url: String,
        format: String,
        key: String,
        product_id: Option<String>,
        position: Option<f64>,
        play: bool,
        expected_gen: u32,
    ) -> anyhow::Result<Continuation> {
        let Some(load_gen) = continue_load(expected_gen) else {
            return Ok(Continuation::Superseded);
        };
        // Honor the caller position directly (live re-arm target, not a
        // resume-store candidate): resume_policy_for would floor sub-1s to Auto
        // and seek a stale persisted offset. Explicit applies the floor itself.
        let policy = match position {
            Some(t) => ResumePolicy::Explicit(t),
            None => ResumePolicy::Disabled,
        };
        self.load_with_policy(url, format, key, product_id, policy, play, load_gen)
            .map(|_| Continuation::Started)
    }

    fn send_cmd(&self, cmd: PlayerCommand) -> anyhow::Result<()> {
        self.cmd_tx
            .send(cmd)
            .map_err(|_| anyhow::anyhow!("Player thread is dead"))
    }

    pub fn play(&self) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::Play)
    }

    pub fn pause(&self) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::Pause)
    }

    /// `scope` for the same reason the loads carry an origin: this bumps the generation too, and
    /// a preload staged after a controller stopped the speaker answers that controller's session.
    /// A scope that does not cover the music playing stops nothing.
    pub fn stop(&self, scope: StopScope) -> anyhow::Result<()> {
        // Pause-retain, not teardown: the in-flight load task and its download are
        // kept (the download rides its own download_cancel token, not the load
        // generation); a same-track re-assert resumes in place. The mint invalidates any
        // still-pending auto-play Load/ReplayRequest (rejected by handle_load's
        // stale-gate and the `continue_load` mint): playback cannot start after the stop;
        // it doesn't truncate the retained download, and a steady-state re-assert
        // resumes via Player::load, which bypasses both gates.
        if mint_if(|_, owner| scope.claim(owner)).is_none() {
            crate::vprintln!("[PLAYER] {scope:?} stop: not this scope's music, left playing");
            return Ok(());
        }
        let event_seq = next_event_seq();
        self.send_cmd(PlayerCommand::Stop(event_seq))
    }

    pub fn seek(&self, time: f64) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::Seek(time))
    }

    /// A page's level, dropped when the page decided it before taking in the level Rust plays
    /// ([`OutputLevel::admits`]). From here on it is a clamped, finite [`Volume`].
    pub fn set_volume(&self, volume: f64, seen: Option<u64>) -> anyhow::Result<()> {
        let volume = Volume::from_percent(volume);
        // Recorded and sent under one lock: a level recorded apart from its send could reach
        // the thread in the other order than the record says, and a mute would then save a
        // level the output never played.
        let mut level = self.output_level.lock().unwrap_or_else(|e| e.into_inner());
        if !level.admits(seen) {
            crate::vprintln!(
                "[VOLUME] page level {:.0}% dropped: decided before report {}",
                volume.as_percent(),
                level.floor
            );
            return Ok(());
        }
        if !level.set_from_page(volume) {
            return Ok(());
        }
        self.keep(*level);
        self.send_cmd(PlayerCommand::SetVolume(volume))
    }

    /// A page that loads takes in the level Rust plays, and gets the report its later levels
    /// carry. Levels decided before, by this page or another, are refused from here on.
    pub fn catch_up_page(&self) -> OutputLevel {
        let mut level = self.output_level.lock().unwrap_or_else(|e| e.into_inner());
        level.catch_up();
        *level
    }

    /// A Connect controller's level. Returns what the output now plays at, for the caller to
    /// report.
    pub fn set_remote_volume(&self, volume: f64) -> anyhow::Result<OutputLevel> {
        let volume = Volume::from_percent(volume);
        let mut level = self.output_level.lock().unwrap_or_else(|e| e.into_inner());
        level.set(volume, LevelWriter::Remote);
        self.keep(*level);
        self.send_cmd(PlayerCommand::SetVolume(volume))?;
        Ok(*level)
    }

    /// Every writer of the level passes here with the lock still held, which keeps the saved
    /// level the newest one: the next launch plays it, whoever set it.
    fn keep(&self, level: OutputLevel) {
        if let Some(store) = &self.level_store {
            store.save(level);
        }
    }

    /// Silences the output, or restores the last audible level it played. Returns what the
    /// output now plays at, which is what the caller reports: nothing else knows the level an
    /// unmute restores.
    pub fn set_mute(&self, mute: bool) -> anyhow::Result<OutputLevel> {
        let mut level = self.output_level.lock().unwrap_or_else(|e| e.into_inner());
        if mute {
            level.mute();
        } else {
            level.unmute();
        }
        self.keep(*level);
        self.send_cmd(PlayerCommand::SetVolume(level.volume()))?;
        Ok(*level)
    }

    /// The OS mixer moved this app's session. The thread already plays that level: it is only
    /// recorded, and the frontend adopts it without sending it back.
    pub fn note_volume(&self, volume: f64) -> OutputLevel {
        let mut level = self.output_level.lock().unwrap_or_else(|e| e.into_inner());
        level.set(Volume::from_percent(volume), LevelWriter::Remote);
        self.keep(*level);
        *level
    }

    pub fn output_level(&self) -> OutputLevel {
        *self.output_level.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(target_os = "windows")]
    pub fn set_volume_sync(&self, enabled: bool) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::SetVolumeSync(enabled))
    }

    pub fn set_crossfade_secs(&self, secs: u8) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::SetCrossfadeSecs(secs))
    }

    pub fn get_audio_devices(&self, req_id: Option<String>) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::GetAudioDevices(req_id))
    }

    pub fn set_audio_device(&self, device_id: String, mode: OutputMode) -> anyhow::Result<()> {
        self.send_cmd(PlayerCommand::SetAudioDevice {
            id: device_id,
            mode,
        })
    }
}

#[cfg(test)]
#[path = "../../tests/unit/player/fixture.rs"]
mod fixture;

#[cfg(test)]
#[path = "../../tests/unit/player/reconcile_tests.rs"]
mod reconcile_tests;

#[cfg(test)]
#[path = "../../tests/unit/player/media_error_code_tests.rs"]
mod media_error_code_tests;

#[cfg(test)]
#[path = "../../tests/unit/player/cache_entry_validation_tests.rs"]
mod cache_entry_validation_tests;

#[cfg(test)]
#[path = "../../tests/unit/player/fetch_url_tests.rs"]
mod fetch_url_tests;

#[cfg(test)]
#[path = "../../tests/unit/player/volume_tests.rs"]
mod volume_tests;

#[cfg(test)]
#[path = "../../tests/unit/player/banner_tests.rs"]
mod banner_tests;

#[cfg(test)]
#[path = "../../tests/unit/player/prebuffer_tests.rs"]
mod prebuffer_tests;

#[cfg(test)]
#[path = "../../tests/unit/player/load_state_tests.rs"]
mod load_state_tests;

#[cfg(test)]
#[path = "../../tests/unit/player/load_failure_text_tests.rs"]
mod load_failure_text_tests;

#[cfg(test)]
#[path = "../../tests/unit/player/heap_probe.rs"]
pub(crate) mod heap_probe;
