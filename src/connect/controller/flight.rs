//! One connect attempt at a time. TIDAL asks for the persisted device at saga start and on every
//! LOGIN_SUCCESS: overlapping requests for one device share one resolve, dial and startSession, and
//! one for another device replaces the attempt in flight, as TIDAL's `takeLatest` on its pick does.
//!
//! Lock order: the slot mutex, then the controller's. The settle step runs under the slot:
//! "this attempt is still the current one" and its install are a single critical section.
//! Nothing that holds the controller or the app state ever takes the slot.

use std::future::Future;
use std::sync::Mutex;

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

struct Flight {
    id: u64,
    fullname: String,
    cancel: CancellationToken,
    done: watch::Sender<Option<ConnectResult>>,
}

struct Slot {
    next_id: u64,
    flight: Option<Flight>,
}

/// The role a request takes in the attempt for its device.
enum Join {
    Lead { id: u64, cancel: CancellationToken },
    Follow(watch::Receiver<Option<ConnectResult>>),
}

/// How a request settled, and whether it was the one that did the work.
pub(crate) struct Settled {
    pub(crate) result: ConnectResult,
    led: bool,
    announce: bool,
}

impl Settled {
    /// Whether TIDAL is told about this failure: once per attempt, by the request that ran it.
    pub(crate) fn announces_failure(&self) -> bool {
        self.announce
    }

    pub(crate) fn led(&self) -> bool {
        self.led
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
            }),
        }
    }

    /// Run the attempt for the device `fullname`, or wait for the request already running it.
    /// `dial` is the part that awaits. `settle` then takes its outcome under the slot lock, and
    /// only while this attempt is still the current one: a replaced attempt hands what it dialled
    /// to `discard` instead and installs nothing.
    pub(crate) async fn run<D>(
        &self,
        fullname: &str,
        dial: impl Future<Output = Result<D, ConnectFailure>>,
        settle: impl FnOnce(Result<D, ConnectFailure>) -> Verdict,
        discard: impl FnOnce(D),
    ) -> Settled {
        match self.join(fullname) {
            Join::Follow(done) => Settled {
                result: wait_for(done).await,
                led: false,
                announce: false,
            },
            Join::Lead { id, cancel } => {
                let _leader = Leader { flights: self, id };
                // Stops dialling for a replaced attempt. Whether it may still install is
                // decided under the lock in `settle_attempt`, not here.
                let dialled = tokio::select! {
                    biased;
                    () = cancel.cancelled() => Err(ConnectFailure::Superseded),
                    dialled = dial => dialled,
                };
                self.settle_attempt(id, dialled, settle, discard)
            }
        }
    }

    fn join(&self, fullname: &str) -> Join {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(flight) = &slot.flight {
            if flight.fullname == fullname {
                return Join::Follow(flight.done.subscribe());
            }
            // The older request loses: its waiters are answered here, its leader wakes on `cancel`.
            flight.cancel.cancel();
            flight.done.send_replace(Some(Err(ConnectFailure::Superseded)));
        }
        slot.next_id += 1;
        let id = slot.next_id;
        let cancel = CancellationToken::new();
        slot.flight = Some(Flight {
            id,
            fullname: fullname.to_string(),
            cancel: cancel.clone(),
            done: watch::channel(None).0,
        });
        Join::Lead { id, cancel }
    }

    /// Install the outcome and hand the answer to the waiters, in one hold of the slot. The
    /// flight leaves the slot only after `settle` returned: a panic in it leaves the flight for
    /// `abandon` to answer.
    fn settle_attempt<D>(
        &self,
        id: u64,
        dialled: Result<D, ConnectFailure>,
        settle: impl FnOnce(Result<D, ConnectFailure>) -> Verdict,
        discard: impl FnOnce(D),
    ) -> Settled {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if !slot.flight.as_ref().is_some_and(|flight| flight.id == id) {
            if let Ok(dialled) = dialled {
                discard(dialled);
            }
            return Settled {
                result: Err(ConnectFailure::Superseded),
                led: true,
                announce: false,
            };
        }
        let verdict = settle(dialled);
        if let Some(flight) = slot.flight.take() {
            flight.done.send_replace(Some(verdict.result.clone()));
        }
        Settled {
            announce: verdict.announce && verdict.result.is_err(),
            result: verdict.result,
            led: true,
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
