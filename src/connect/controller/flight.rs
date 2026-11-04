//! One connect attempt at a time. TIDAL asks for the persisted device at saga start and on every
//! LOGIN_SUCCESS: overlapping requests for one device share one resolve, dial and the device's answer
//! to its start or resume, and one for another device replaces the attempt in flight, as TIDAL's
//! `takeLatest` on its pick does.
//!
//! Lock order: the slot mutex, then the app state, then the controller's. The settle step runs
//! under the slot: "still the current attempt" and its install are one critical section. Nothing
//! that holds the controller or the app state ever takes the slot.

use std::future::Future;
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::dial::ConnectFailure;

/// What a connect request ended in: whether the device already held our session (`joined`).
pub(crate) type ConnectResult = Result<bool, ConnectFailure>;

/// What the settle step decided: the answer, and whether TIDAL is told of a failure.
pub(crate) struct Verdict {
    pub(crate) result: ConnectResult,
    pub(crate) announce: bool,
}

impl Verdict {
    pub(crate) fn done(joined: bool) -> Self {
        Self {
            result: Ok(joined),
            announce: false,
        }
    }
}

/// Stops a leader's dial, and says what its request ends in.
struct Cancel {
    token: CancellationToken,
    reason: Mutex<Option<ConnectFailure>>,
}

impl Cancel {
    fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            reason: Mutex::new(None),
        }
    }

    fn fire(&self, reason: ConnectFailure) {
        *self.reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
        self.token.cancel();
    }

    /// Why the attempt was stopped: replaced, cancelled or closed. One nothing stopped ended
    /// without an answer, its leader having died or the exit having overtaken its claim.
    fn reason(&self) -> ConnectFailure {
        self.reason
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or(ConnectFailure::Aborted)
    }
}

struct Flight {
    id: u64,
    fullname: String,
    cancel: Arc<Cancel>,
    done: watch::Sender<Option<ConnectResult>>,
}

struct Slot {
    next_id: u64,
    flight: Option<Flight>,
    /// Set by `close`: no request joins any more, each is answered as aborted.
    closed: bool,
    /// The abort hook of every request still running, with its part: what `close` answers.
    hooks: Vec<(Weak<Hook>, Part)>,
}

/// The role a request takes in the attempt for its device.
enum Join {
    Lead { id: u64, cancel: Arc<Cancel> },
    Follow(watch::Receiver<Option<ConnectResult>>),
    Closed,
}

/// The part a request has in the attempt for its device. A leader reads how its own attempt
/// ended off its `Cancel`, which may no longer be the attempt in flight.
#[derive(Clone)]
enum Part {
    Leader(Arc<Cancel>),
    Follower,
}

impl Part {
    fn unanswered(&self) -> Unanswered {
        match self {
            Self::Leader(cancel) => Unanswered::Led(cancel.reason()),
            Self::Follower => Unanswered::Followed,
        }
    }
}

/// What a request that ends without having answered itself is told: that it ran an attempt, and
/// why that attempt stopped, a failure that is its own to report; or that it only waited on one.
pub(crate) enum Unanswered {
    Led(ConnectFailure),
    Followed,
}

type Abort = Box<dyn FnOnce(Unanswered) + Send>;

/// What answers a request that ends without having answered itself.
type Hook = Mutex<Option<Abort>>;

/// The request's abort hook, run on a panic or a cancelled task (the query would otherwise stay
/// pending for good) and by `close` at exit. Whichever takes it first is the only one to run it.
struct Armed {
    hook: Arc<Hook>,
    part: Part,
}

impl Armed {
    fn take(hook: &Hook) -> Option<Abort> {
        hook.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        if let Some(abort) = Self::take(&self.hook) {
            abort(self.part.unanswered());
        }
    }
}

/// How a request settled, and whether it was the one that did the work.
pub(crate) struct Settled {
    pub(crate) result: ConnectResult,
    announce: bool,
    armed: Armed,
}

impl Settled {
    /// The caller takes the answer of its own request. False when `close` gave it already: the
    /// caller then answers nothing, its callback is spent.
    #[must_use = "a request that answers without claiming can answer twice"]
    pub(crate) fn claim(&mut self) -> bool {
        Armed::take(&self.armed.hook).is_some()
    }

    /// Whether TIDAL is told about this failure: once per attempt, by the request that ran it.
    pub(crate) fn announces_failure(&self) -> bool {
        self.announce
    }

    pub(crate) fn led(&self) -> bool {
        matches!(self.armed.part, Part::Leader(_))
    }
}

/// Removes the leader's flight on every way out of `run`, panic and task abort included: a flight
/// left in the slot would make every later request for the device wait on a leader that is gone.
struct Leader<'a> {
    flights: &'a ConnectFlights,
    id: u64,
}

impl Drop for Leader<'_> {
    fn drop(&mut self) {
        self.flights.abandon(self.id);
    }
}

pub(crate) struct ConnectFlights {
    slot: Mutex<Slot>,
}

impl ConnectFlights {
    pub(crate) const fn new() -> Self {
        Self {
            slot: Mutex::new(Slot {
                next_id: 0,
                flight: None,
                closed: false,
                hooks: Vec::new(),
            }),
        }
    }

    /// Run the attempt for `fullname`, or wait for the request already running it. `dial` awaits;
    /// `settle` takes its outcome under the slot lock while the attempt is current (a replaced one
    /// hands what it dialled to `discard`); `abort` runs if the request ends unclaimed, or on `close`.
    pub(crate) async fn run<D>(
        &self,
        fullname: &str,
        dial: impl Future<Output = Result<D, ConnectFailure>>,
        settle: impl FnOnce(Result<D, ConnectFailure>) -> Verdict,
        discard: impl FnOnce(D),
        abort: impl FnOnce(Unanswered) + Send + 'static,
    ) -> Settled {
        let hook: Arc<Hook> = Arc::new(Mutex::new(Some(Box::new(abort))));
        match self.join(fullname, &hook) {
            Join::Closed => Settled {
                result: Err(ConnectFailure::Aborted),
                announce: false,
                armed: Armed {
                    hook,
                    part: Part::Follower,
                },
            },
            Join::Follow(done) => {
                let armed = Armed {
                    hook,
                    part: Part::Follower,
                };
                Settled {
                    result: wait_for(done).await,
                    announce: false,
                    armed,
                }
            }
            Join::Lead { id, cancel } => {
                let armed = Armed {
                    hook,
                    part: Part::Leader(cancel.clone()),
                };
                let _leader = Leader { flights: self, id };
                // Stops dialling for a replaced attempt. Whether it may still install is
                // decided under the lock in `settle_attempt`, not here.
                let dialled = tokio::select! {
                    biased;
                    () = cancel.token.cancelled() => Err(cancel.reason()),
                    dialled = dial => dialled,
                };
                self.settle_attempt(id, &cancel, dialled, settle, discard, armed)
            }
        }
    }

    /// Registers `hook` with the role the request takes, in the same hold of the slot that
    /// decides the role: `close` then sees either the request's hook or the closed flag.
    fn join(&self, fullname: &str, hook: &Arc<Hook>) -> Join {
        let mut guard = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        let slot = &mut *guard;
        if slot.closed {
            return Join::Closed;
        }
        slot.hooks
            .retain(|(registered, _)| registered.strong_count() > 0);
        if let Some(flight) = &slot.flight {
            if flight.fullname == fullname {
                let done = flight.done.subscribe();
                slot.hooks.push((Arc::downgrade(hook), Part::Follower));
                return Join::Follow(done);
            }
            // The older request loses: its waiters are answered here, its leader wakes on `cancel`.
            flight.cancel.fire(ConnectFailure::Superseded);
            flight
                .done
                .send_replace(Some(Err(ConnectFailure::Superseded)));
        }
        slot.next_id += 1;
        let id = slot.next_id;
        let cancel = Arc::new(Cancel::new());
        slot.flight = Some(Flight {
            id,
            fullname: fullname.to_string(),
            cancel: cancel.clone(),
            done: watch::channel(None).0,
        });
        slot.hooks
            .push((Arc::downgrade(hook), Part::Leader(cancel.clone())));
        Join::Lead { id, cancel }
    }

    /// Settles every request still running, for the exit: each one's abort hook runs now, while
    /// what it answers through is alive, and the request then claims nothing. Later requests are
    /// answered as aborted. The hooks run outside the slot, which they have no business in. A
    /// leader replaced or cancelled earlier still ends as its attempt did, not as aborted.
    pub(crate) fn close(&self) {
        let hooks = {
            let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
            slot.closed = true;
            if let Some(flight) = slot.flight.take() {
                flight.cancel.fire(ConnectFailure::Aborted);
                flight.done.send_replace(Some(Err(ConnectFailure::Aborted)));
            }
            std::mem::take(&mut slot.hooks)
        };
        for (hook, part) in hooks {
            if let Some(abort) = hook.upgrade().and_then(|hook| Armed::take(&hook)) {
                abort(part.unanswered());
            }
        }
    }

    /// Ends the attempt in flight, as a disconnect does: nothing it dials is installed or
    /// announced, and every request waiting on it is answered `Cancelled`. A leader already past
    /// its settle step has installed what the disconnect then tears down.
    pub(crate) fn cancel(&self) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(flight) = slot.flight.take() {
            flight.cancel.fire(ConnectFailure::Cancelled);
            flight
                .done
                .send_replace(Some(Err(ConnectFailure::Cancelled)));
        }
    }

    /// Install the outcome and hand the answer to the waiters, in one hold of the slot. The
    /// flight leaves the slot only after `settle` returned: a panic in it leaves the flight for
    /// `abandon` to answer.
    fn settle_attempt<D>(
        &self,
        id: u64,
        cancel: &Cancel,
        dialled: Result<D, ConnectFailure>,
        settle: impl FnOnce(Result<D, ConnectFailure>) -> Verdict,
        discard: impl FnOnce(D),
        armed: Armed,
    ) -> Settled {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if !slot.flight.as_ref().is_some_and(|flight| flight.id == id) {
            if let Ok(dialled) = dialled {
                discard(dialled);
            }
            return Settled {
                result: Err(cancel.reason()),
                announce: false,
                armed,
            };
        }
        let verdict = settle(dialled);
        if let Some(flight) = slot.flight.take() {
            flight.done.send_replace(Some(verdict.result.clone()));
        }
        Settled {
            announce: verdict.announce && verdict.result.is_err(),
            result: verdict.result,
            armed,
        }
    }

    /// Answer the waiters of a flight whose leader ended without settling it. A no-op once the
    /// leader settled or was replaced: the slot then holds another flight, or none.
    fn abandon(&self, id: u64) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(flight) = slot.flight.take_if(|flight| flight.id == id) {
            flight.done.send_replace(Some(Err(ConnectFailure::Aborted)));
        }
    }
}

async fn wait_for(mut done: watch::Receiver<Option<ConnectResult>>) -> ConnectResult {
    match done.wait_for(Option::is_some).await {
        Ok(settled) => settled.clone().unwrap_or(Err(ConnectFailure::Aborted)),
        Err(_) => Err(ConnectFailure::Aborted),
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/connect/controller/flight.rs"]
mod tests;
