use std::sync::atomic::{
    AtomicU32, AtomicU64,
    Ordering::{AcqRel, Acquire, Relaxed},
};

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

    /// Whether the music this stop halts is left with no page behind it: a page's session or
    /// document ended. A stop is pause-retain, and whoever presses Play next would meet a track no
    /// page loaded.
    pub(crate) fn orphans(self) -> bool {
        match self {
            StopScope::OwnMusic => true,
            StopScope::As(_) | StopScope::Playing | StopScope::Cast => false,
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
pub(super) fn mint_if(claim: impl Fn(u32, LoadOrigin) -> Option<LoadOrigin>) -> Option<u32> {
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

#[cfg(test)]
#[path = "../../tests/unit/player/load_state.rs"]
mod tests;
