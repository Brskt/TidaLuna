use std::path::PathBuf;

use super::install::{
    CodecTarget, FetchError, Verified, current_target, fetch_and_install, installed_path, load_api,
    verify_installed,
};
use super::{FailureClass, off_runtime};

/// Where the decoder stands, as last settled. The supervisor's task is its only writer: the
/// `watch::Sender` lives in that task alone, and nothing else in the process can publish.
#[derive(Debug, Clone, PartialEq)]
enum Phase {
    /// Nothing has been proven yet.
    Starting,
    Downloading,
    Ready(PathBuf),
    Unavailable(Unavailable),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unavailable {
    Off,
    Unsupported,
    Failed(FailureClass),
}

/// Why a cover gets no decoder: what the supervisor published, or a supervisor that is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoDecoder {
    Unavailable(Unavailable),
    /// Never started, or its task ended with the runtime: a step that panics fails `Crashed`.
    Stopped,
}

impl NoDecoder {
    /// A short phrase for the UI, not a log line. `'static`: no cause can be formatted into it.
    pub(crate) fn phrase(self) -> &'static str {
        match self {
            Self::Unavailable(Unavailable::Off) => "video decoding is turned off in settings",
            Self::Unavailable(Unavailable::Unsupported) => {
                "no video codec is published for this platform"
            }
            Self::Unavailable(Unavailable::Failed(class)) => class.phrase(),
            Self::Stopped => "the video codec service has stopped",
        }
    }
}

/// An ask answered from the phase. `Starting` and `Downloading` only outlive a step once the
/// supervisor is gone: a live one answers after it settled.
fn answer(phase: &Phase) -> Result<PathBuf, NoDecoder> {
    match phase {
        Phase::Ready(path) => Ok(path.clone()),
        Phase::Unavailable(u) => Err(NoDecoder::Unavailable(*u)),
        Phase::Starting | Phase::Downloading => Err(NoDecoder::Stopped),
    }
}

/// What the supervisor publishes. `seq` moves with every change; the page keeps the newest of
/// a push and a pull that crossed: they travel different paths.
#[derive(Debug, Clone, PartialEq)]
struct Snapshot {
    seq: u64,
    /// The user's switch, as the backend holds it.
    enabled: bool,
    phase: Phase,
}

fn publish(tx: &tokio::sync::watch::Sender<Snapshot>, phase: Phase, enabled: bool) {
    tx.send_if_modified(|current| {
        if current.phase == phase && current.enabled == enabled {
            return false;
        }
        current.phase = phase;
        current.enabled = enabled;
        current.seq += 1;
        true
    });
}

/// How a failure gets asked again, as the settings row tells it. A named enum rather than a
/// `retrying` bool: "on the next demand" and "never" want different words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Recovery {
    Timer,
    OnDemand,
    Never,
}

impl FailureClass {
    fn recovery(self) -> Recovery {
        match self {
            Self::Network => Recovery::Timer,
            Self::Refused | Self::Corrupt | Self::Disk | Self::Crashed => Recovery::OnDemand,
            Self::Unloadable => Recovery::Never,
        }
    }
}

/// The page's copy of a [`Snapshot`], with no path and only `'static` text: no cause leaves the
/// process this way either.
#[derive(Debug, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PhaseView {
    Starting,
    Off,
    Unsupported,
    Downloading,
    Ready,
    Failed {
        reason: &'static str,
        recovery: Recovery,
    },
}

#[derive(Debug, PartialEq, serde::Serialize)]
pub(crate) struct SnapshotView {
    seq: u64,
    enabled: bool,
    phase: PhaseView,
}

fn view(snapshot: &Snapshot) -> SnapshotView {
    let phase = match snapshot.phase {
        Phase::Starting => PhaseView::Starting,
        Phase::Downloading => PhaseView::Downloading,
        Phase::Ready(_) => PhaseView::Ready,
        Phase::Unavailable(Unavailable::Off) => PhaseView::Off,
        Phase::Unavailable(Unavailable::Unsupported) => PhaseView::Unsupported,
        Phase::Unavailable(Unavailable::Failed(class)) => PhaseView::Failed {
            reason: class.phrase(),
            recovery: class.recovery(),
        },
    };
    SnapshotView {
        seq: snapshot.seq,
        enabled: snapshot.enabled,
        phase,
    }
}

/// The event the page listens to, and the request it pulls with at subscription.
const STATE_CHANNEL: &str = "settings.video_codec_state";

/// A change published before the page listens is lost here; the page's pull at subscription
/// makes up for it.
async fn emit_changes(mut state: tokio::sync::watch::Receiver<Snapshot>) {
    while state.changed().await.is_ok() {
        let view = view(&state.borrow_and_update());
        crate::app_state::emit_ipc_event_with_data(STATE_CHANNEL, &view);
    }
}

const FIRST_RETRY: std::time::Duration = std::time::Duration::from_secs(30);
const MAX_RETRY: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Doubles up to `MAX_RETRY`, then holds: a machine offline for the day costs one request per
/// half hour.
fn next_retry_delay(delay: std::time::Duration) -> std::time::Duration {
    delay.saturating_mul(2).min(MAX_RETRY)
}

/// How often a demand may ask again after a `Network` failure. A fixed floor rather than the
/// backoff, since a demand proves TIDAL's network works yet says nothing of Cisco's host (a
/// firewall blocks it while TIDAL loads). It spaces the demands only: the timer keeps its own
/// backoff, whose first attempts come closer together than this.
const NETWORK_DEMAND_FLOOR: std::time::Duration = std::time::Duration::from_secs(5 * 60);

impl FailureClass {
    /// Only a lost network heals by itself; only it has a timer asking again.
    fn timer(self) -> bool {
        match self {
            Self::Network => true,
            Self::Refused | Self::Corrupt | Self::Disk | Self::Unloadable | Self::Crashed => false,
        }
    }

    /// How long after the last attempt a demand may attempt again, `wait` being the backoff that
    /// attempt earned. `None`: a fact about this process; only a relaunch or the user's switch
    /// changes it.
    fn demand_floor(self, wait: std::time::Duration) -> Option<std::time::Duration> {
        match self {
            Self::Network => Some(NETWORK_DEMAND_FLOOR),
            // An attempt may cost a download or a hash: it rides the backoff.
            Self::Refused | Self::Corrupt | Self::Disk | Self::Crashed => Some(wait),
            Self::Unloadable => None,
        }
    }
}

/// What woke the supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    Boot,
    Timer,
    /// A cover job asking, or a request the artwork filter armed for.
    Demand,
    /// A decode could not load the file this phase called ready.
    Recheck,
    /// The user moved the switch: the manual retry of any failure.
    Switch,
}

/// What a step does. A named enum: `plan` is a table the tests read whole.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    Off,
    Unsupported,
    Verify,
    Hold,
}

/// `due`: the timer's own deadline has passed; a stale wake-up cannot beat a backoff a later
/// attempt extended. `since`: time since the last attempt, whoever made it. `wait`: the backoff
/// that attempt earned.
fn plan(
    phase: &Phase,
    wanted: bool,
    target_known: bool,
    why: Why,
    due: bool,
    since: Option<std::time::Duration>,
    wait: std::time::Duration,
) -> Plan {
    if !wanted {
        return Plan::Off;
    }
    if !target_known {
        return Plan::Unsupported;
    }
    match phase {
        Phase::Starting | Phase::Unavailable(Unavailable::Off) => Plan::Verify,
        Phase::Downloading | Phase::Unavailable(Unavailable::Unsupported) => Plan::Hold,
        Phase::Ready(_) => match why {
            Why::Recheck => Plan::Verify,
            Why::Boot | Why::Timer | Why::Demand | Why::Switch => Plan::Hold,
        },
        Phase::Unavailable(Unavailable::Failed(class)) => match why {
            // An off and an on the task read as one level still ask for the retry.
            Why::Switch => Plan::Verify,
            Why::Timer if class.timer() && due => Plan::Verify,
            Why::Demand
                if class
                    .demand_floor(wait)
                    .is_some_and(|floor| since.is_none_or(|d| d >= floor)) =>
            {
                Plan::Verify
            }
            Why::Boot | Why::Timer | Why::Demand | Why::Recheck => Plan::Hold,
        },
    }
}

/// Whether a failure is news. The same class again is the state the page already shows, and
/// `verr!` is budgeted; a different one is.
#[derive(Debug, PartialEq, Eq)]
enum Gate {
    Loud,
    Quiet,
}

fn log_gate(last: Option<FailureClass>, new: FailureClass) -> Gate {
    if last == Some(new) {
        Gate::Quiet
    } else {
        Gate::Loud
    }
}

/// The supervisor's whole contact with the disk, the network and the loader, which the tests
/// script to run a day in paused time.
trait Io: Send + Sync + 'static {
    fn target_known(&self) -> bool;
    /// `None`: the runtime is shutting down.
    fn verify(&self) -> impl Future<Output = Option<Verified>> + Send;
    fn fetch(&self) -> impl Future<Output = Result<PathBuf, FetchError>> + Send;
    /// `None`: the runtime is shutting down.
    fn probe(&self, path: PathBuf) -> impl Future<Output = Option<anyhow::Result<()>>> + Send;
    fn report(&self, gate: Gate, err: anyhow::Error);
}

struct OnDisk {
    target: Option<&'static CodecTarget>,
}

impl Io for OnDisk {
    fn target_known(&self) -> bool {
        self.target.is_some()
    }

    async fn verify(&self) -> Option<Verified> {
        // `plan` never verifies without a target.
        let target = self.target?;
        off_runtime(move || verify_installed(&installed_path(target), target)).await
    }

    async fn fetch(&self) -> Result<PathBuf, FetchError> {
        // `plan` never fetches without a target; were it to, there is no build to ask for.
        let target = self.target.ok_or(FetchError::NoTarget)?;
        let path = fetch_and_install(target).await?;
        crate::vprintln!("[VIDEO]   Codec installed ({})", path.display());
        Ok(path)
    }

    /// Each decode loads its own copy (`Decoder` takes the API by value); the probe's handle
    /// is dropped here.
    async fn probe(&self, path: PathBuf) -> Option<anyhow::Result<()>> {
        off_runtime(move || load_api(&path).map(drop)).await
    }

    fn report(&self, gate: Gate, err: anyhow::Error) {
        match gate {
            Gate::Loud => crate::verr!("[VIDEO]   {err:#}"),
            Gate::Quiet => crate::vprintln!("[VIDEO]   {err:#}"),
        }
    }
}

/// The supervisor's memory, which only its task touches.
struct Core<I: Io> {
    io: I,
    /// The backoff the last failure earned.
    wait: std::time::Duration,
    failures: u32,
    last_attempt: Option<tokio::time::Instant>,
    /// When the timer asks again; `None` for a class without one.
    next_at: Option<tokio::time::Instant>,
    last_failure: Option<FailureClass>,
    /// Whether the verified library loads in this process, asked once until a switch click clears
    /// it. The digest pins the file; what can still refuse it is the environment (a loader
    /// dependency the system lacks, a glibc too old).
    loads: Option<bool>,
}

impl<I: Io> Core<I> {
    fn new(io: I) -> Self {
        Self {
            io,
            wait: FIRST_RETRY,
            failures: 0,
            last_attempt: None,
            next_at: None,
            last_failure: None,
            loads: None,
        }
    }

    /// One step, a panic anywhere in it ending as a published `Crashed` (the blocking closures
    /// re-raise theirs through `off_runtime`). The task keeps serving: dead, it would leave the
    /// last phase published for the rest of the process, a `Ready` whose file is gone included.
    async fn step(&mut self, tx: &tokio::sync::watch::Sender<Snapshot>, wanted: bool, why: Why) {
        use futures_util::FutureExt;
        if let Err(payload) = std::panic::AssertUnwindSafe(self.run_step(tx, wanted, why))
            .catch_unwind()
            .await
        {
            self.crashed(tx, wanted, payload);
        }
    }

    async fn run_step(
        &mut self,
        tx: &tokio::sync::watch::Sender<Snapshot>,
        wanted: bool,
        why: Why,
    ) {
        let phase = tx.borrow().phase.clone();
        let since = self.last_attempt.map(|t| t.elapsed());
        let due = self
            .next_at
            .is_some_and(|t| tokio::time::Instant::now() >= t);
        match plan(
            &phase,
            wanted,
            self.io.target_known(),
            why,
            due,
            since,
            self.wait,
        ) {
            Plan::Hold => {}
            Plan::Off => {
                self.next_at = None;
                publish(tx, Phase::Unavailable(Unavailable::Off), wanted);
            }
            Plan::Unsupported => {
                publish(tx, Phase::Unavailable(Unavailable::Unsupported), wanted);
            }
            Plan::Verify => {
                if why == Why::Switch {
                    // The user's click is the manual retry: nothing remembered holds it back,
                    // not even a library that would not load (a system library may have changed).
                    self.failures = 0;
                    self.wait = FIRST_RETRY;
                    self.last_attempt = None;
                    self.next_at = None;
                    self.loads = None;
                }
                if let Some(next) = self.settle(tx, wanted).await {
                    publish(tx, next, wanted);
                }
            }
        }
    }

    /// Verify, fetch if needed, probe once. `None` when the runtime shut down underneath: nothing
    /// was decided or published.
    async fn settle(
        &mut self,
        tx: &tokio::sync::watch::Sender<Snapshot>,
        wanted: bool,
    ) -> Option<Phase> {
        self.last_attempt = Some(tokio::time::Instant::now());
        let path = match self.io.verify().await? {
            Verified::Installed(path) => path,
            Verified::Stuck(e) => {
                let err = anyhow::Error::from(e)
                    .context("the installed codec is unusable and cannot be replaced");
                return Some(self.failed(FailureClass::Disk, err));
            }
            // Both leave nothing at the path, which is what the fetch downloads onto.
            Verified::Absent | Verified::Removed => {
                publish(tx, Phase::Downloading, wanted);
                match self.io.fetch().await {
                    Ok(path) => path,
                    Err(e) => {
                        let class = e.class()?;
                        let err = anyhow::Error::from(e).context("codec install failed");
                        return Some(self.failed(class, err));
                    }
                }
            }
        };
        // A `Ready` the digest alone vouches for is loaded once before it is believed: a library
        // that hashes right but cannot load would otherwise disarm the artwork filter for covers
        // no decode can paint.
        let loads = match self.loads {
            Some(loads) => loads,
            None => {
                let loads = match self.io.probe(path.clone()).await? {
                    Ok(()) => true,
                    Err(e) => {
                        let err = e.context("the codec does not load on this system");
                        let gate = log_gate(self.last_failure, FailureClass::Unloadable);
                        self.io.report(gate, err);
                        false
                    }
                };
                self.loads = Some(loads);
                loads
            }
        };
        if !loads {
            self.last_failure = Some(FailureClass::Unloadable);
            self.next_at = None;
            return Some(Phase::Unavailable(Unavailable::Failed(
                FailureClass::Unloadable,
            )));
        }
        self.last_failure = None;
        self.failures = 0;
        self.wait = FIRST_RETRY;
        self.next_at = None;
        Some(Phase::Ready(path))
    }

    /// Published before anything is logged, since the logger may be what panicked, and the log
    /// is guarded itself: a second panic there must not end the task the first one spared.
    fn crashed(
        &mut self,
        tx: &tokio::sync::watch::Sender<Snapshot>,
        wanted: bool,
        payload: Box<dyn std::any::Any + Send>,
    ) {
        // A panic before `settle` stamped it would leave `None`, which `plan` reads as a floor
        // already passed.
        self.last_attempt = Some(tokio::time::Instant::now());
        let gate = log_gate(self.last_failure, FailureClass::Crashed);
        let phase = self.record_failure(FailureClass::Crashed);
        publish(tx, phase, wanted);
        let cause = crate::connect::runtime::panic_payload_to_string(payload);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.io.report(
                gate,
                anyhow::anyhow!("a codec supervisor step panicked: {cause}"),
            );
        }));
    }

    fn failed(&mut self, class: FailureClass, err: anyhow::Error) -> Phase {
        self.io.report(log_gate(self.last_failure, class), err);
        self.record_failure(class)
    }

    fn record_failure(&mut self, class: FailureClass) -> Phase {
        self.last_failure = Some(class);
        self.wait = if self.failures == 0 {
            FIRST_RETRY
        } else {
            next_retry_delay(self.wait)
        };
        self.failures += 1;
        self.next_at = class
            .timer()
            .then(|| tokio::time::Instant::now() + self.wait);
        Phase::Unavailable(Unavailable::Failed(class))
    }
}

enum Wake {
    Ask(tokio::sync::oneshot::Sender<Result<PathBuf, NoDecoder>>),
    Recheck,
    /// The switch moved. The task reads `wanted` as a level; a burst of toggles settles once, on
    /// the last position. The sender gets the snapshot after the step that took the wake; dropped
    /// unanswered, it refuses the click.
    Switch(SwitchReply),
}

type SwitchReply = tokio::sync::oneshot::Sender<SnapshotView>;

/// The task's end of the wake channel. A switch is a level, and the ones queued back to back are
/// one retry (each would otherwise plan its own `Verify` and fetch a failure once per click).
/// Only a run is folded; a switch behind an ask or a recheck keeps its place.
struct Wakes {
    rx: tokio::sync::mpsc::UnboundedReceiver<Wake>,
    /// The first message after a run of switches, already taken off the channel.
    held: Option<Wake>,
}

impl Wakes {
    async fn recv(&mut self) -> Option<Wake> {
        match self.held.take() {
            Some(wake) => Some(wake),
            None => self.rx.recv().await,
        }
    }

    /// Folds the switches queued right behind the one just taken, keeping their senders: each is
    /// answered after the one step the run plans.
    fn fold_switches(&mut self, replies: &mut Vec<SwitchReply>) {
        while let Ok(wake) = self.rx.try_recv() {
            match wake {
                Wake::Switch(reply) => replies.push(reply),
                other => {
                    self.held = Some(other);
                    break;
                }
            }
        }
    }
}

/// The handle every caller holds. It carries a `Receiver` and the ways to wake the task, never the
/// `Sender`. Its entries are plain `send`, `store` and `notify_one`, callable from CEF's IO
/// thread, the UI thread or a blocking task with no runtime context.
struct Supervisor {
    state: tokio::sync::watch::Receiver<Snapshot>,
    wakes: tokio::sync::mpsc::UnboundedSender<Wake>,
    demand: std::sync::Arc<tokio::sync::Notify>,
    wanted: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Supervisor {
    fn start<I: Io>(rt: &tokio::runtime::Handle, io: I, wanted: bool) -> Self {
        let (tx, state) = tokio::sync::watch::channel(Snapshot {
            seq: 0,
            enabled: wanted,
            phase: Phase::Starting,
        });
        let (wakes, rx) = tokio::sync::mpsc::unbounded_channel();
        let rx = Wakes { rx, held: None };
        let demand = std::sync::Arc::new(tokio::sync::Notify::new());
        let wanted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(wanted));
        rt.spawn(supervise(
            Core::new(io),
            tx,
            rx,
            demand.clone(),
            wanted.clone(),
        ));
        Self {
            state,
            wakes,
            demand,
            wanted,
        }
    }

    /// A download under way finishes first: the task is busy with it, and `Off` follows.
    fn set_enabled(&self, on: bool) -> tokio::sync::oneshot::Receiver<SnapshotView> {
        let (reply, answered) = tokio::sync::oneshot::channel();
        self.wanted.store(on, std::sync::atomic::Ordering::Release);
        // A task already gone hands the wake back with its sender, dropped here: the refusal.
        let _ = self.wakes.send(Wake::Switch(reply));
        answered
    }

    fn is_ready(&self) -> bool {
        matches!(self.state.borrow().phase, Phase::Ready(_))
    }

    /// Asks share the task's queue: fifty cover jobs at once are one fetch and fifty answers.
    async fn ask(&self) -> Result<PathBuf, NoDecoder> {
        if let Phase::Ready(path) = &self.state.borrow().phase {
            return Ok(path.clone());
        }
        let (reply, answered) = tokio::sync::oneshot::channel();
        if self.wakes.send(Wake::Ask(reply)).is_err() {
            return answer(&self.state.borrow().phase);
        }
        answered
            .await
            .unwrap_or_else(|_| answer(&self.state.borrow().phase))
    }

    /// `notify_one` stores a single permit; a burst of requests wakes the task once. Nothing to
    /// wake for unless something failed.
    fn demand(&self) {
        if matches!(
            self.state.borrow().phase,
            Phase::Unavailable(Unavailable::Failed(_))
        ) {
            self.demand.notify_one();
        }
    }

    fn recheck(&self) {
        // A task already gone has nothing to recheck.
        let _ = self.wakes.send(Wake::Recheck);
    }
}

async fn supervise<I: Io>(
    mut core: Core<I>,
    tx: tokio::sync::watch::Sender<Snapshot>,
    mut wakes: Wakes,
    demand: std::sync::Arc<tokio::sync::Notify>,
    wanted: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let wanted = || wanted.load(std::sync::atomic::Ordering::Acquire);
    core.step(&tx, wanted(), Why::Boot).await;
    loop {
        let next_at = core.next_at;
        let timer = async move {
            match next_at {
                Some(t) => tokio::time::sleep_until(t).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            wake = wakes.recv() => match wake {
                None => return,
                Some(Wake::Ask(reply)) => {
                    core.step(&tx, wanted(), Why::Demand).await;
                    let _ = reply.send(answer(&tx.borrow().phase));
                }
                Some(Wake::Recheck) => core.step(&tx, wanted(), Why::Recheck).await,
                Some(Wake::Switch(first)) => {
                    let mut replies = vec![first];
                    wakes.fold_switches(&mut replies);
                    core.step(&tx, wanted(), Why::Switch).await;
                    for reply in replies {
                        let _ = reply.send(view(&tx.borrow()));
                    }
                }
            },
            () = demand.notified() => core.step(&tx, wanted(), Why::Demand).await,
            () = timer => core.step(&tx, wanted(), Why::Timer).await,
        }
    }
}

static SUPERVISOR: std::sync::OnceLock<Supervisor> = std::sync::OnceLock::new();

/// Starts the one supervisor, whatever the switch says: with it off, it answers `Off` and fetches
/// nothing. Fetched at launch, as Firefox does, rather than on the first animated cover: until the
/// library is proven the artwork filter keeps the now-playing view on still covers. Waiting for
/// a cover to ask would leave it still until an album page happened to.
pub(crate) fn start(rt: &tokio::runtime::Handle, enabled: bool) {
    SUPERVISOR.get_or_init(|| {
        let io = OnDisk {
            target: current_target(),
        };
        let supervisor = Supervisor::start(rt, io, enabled);
        rt.spawn(emit_changes(supervisor.state.clone()));
        supervisor
    });
}

/// The page's pull, answered from the published snapshot: a `borrow` and serde, no IO; it
/// does not block the UI thread it runs on. `null` before [`start`], which the page ignores.
pub(crate) fn state_json() -> String {
    SUPERVISOR
        .get()
        .and_then(|supervisor| serde_json::to_string(&view(&supervisor.state.borrow())).ok())
        .unwrap_or_else(|| "null".to_string())
}

/// The artwork filter reads it once per request. False until the library is proven (TIDAL
/// then serves the still cover), and never on a platform or network where it cannot be.
pub(crate) fn is_ready() -> bool {
    SUPERVISOR.get().is_some_and(Supervisor::is_ready)
}

/// The decoder's path for a cover job, fetching it first when the schedule allows.
pub(crate) async fn decoder() -> Result<PathBuf, NoDecoder> {
    match SUPERVISOR.get() {
        Some(supervisor) => supervisor.ask().await,
        None => Err(NoDecoder::Stopped),
    }
}

/// A request the artwork filter armed for: someone wants animated covers. Free unless the codec
/// failed, and then rate-limited by the class's floor.
pub(crate) fn note_demand() {
    if let Some(supervisor) = SUPERVISOR.get() {
        supervisor.demand();
    }
}

/// The user's switch, live. Off keeps the installed file (Firefox removes nothing either); on
/// again only verifies it.
pub(crate) fn set_enabled(on: bool) -> tokio::sync::oneshot::Receiver<SnapshotView> {
    match SUPERVISOR.get() {
        Some(supervisor) => supervisor.set_enabled(on),
        // Never started: the sender dropped here is the refusal.
        None => tokio::sync::oneshot::channel().1,
    }
}

/// A decode could not load the file the supervisor called ready: verify it again.
pub(crate) fn recheck() {
    if let Some(supervisor) = SUPERVISOR.get() {
        supervisor.recheck();
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/video/codec/supervisor.rs"]
mod tests;
