//! Pending trust prompts, one slot per "module::rows" (the rows the dialog shows). A slot is the
//! identity of one dialog: its opener, its waiters and the answer hold the slot, never a lookup
//! key. It ends answered, or `Abandoned` (dropped by the system or closed by the user; no decision).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Decision {
    /// One answer per row, in the order the dialog showed them: true allows.
    Answered(Arc<[bool]>),
    /// Dropped with no answer (uninstall, a newer load, a dialog closed or never opened); nothing
    /// may be recorded as if somebody had answered.
    Abandoned,
}

struct SlotState {
    tx: watch::Sender<Option<Decision>>,
}

impl SlotState {
    fn new() -> Arc<Self> {
        let (tx, _rx) = watch::channel(None);
        Arc::new(Self { tx })
    }

    /// First verdict wins: an answer after the slot ended changes nothing.
    fn settle(&self, decision: Decision) {
        self.tx.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(decision);
            true
        });
    }

    /// Overrides a decision nobody has acted on yet: abandonment is the only verdict that may.
    fn abandon(&self) {
        self.tx.send_if_modified(|current| {
            if *current == Some(Decision::Abandoned) {
                return false;
            }
            *current = Some(Decision::Abandoned);
            true
        });
    }

    fn current(&self) -> Option<Decision> {
        self.tx.borrow().clone()
    }
}

struct Slot {
    /// The load of the plugin that opened this dialog; a dialog is only shared within one load.
    load_id: u64,
    state: Arc<SlotState>,
}

#[derive(Default)]
pub(super) struct Prompts {
    slots: Mutex<HashMap<String, Slot>>,
}

/// What `enter` hands back: where to wait, and, for the attempt that has to open the dialog, the
/// handle that carries the answer back.
pub(super) struct Entered {
    pub(super) waiting: Waiting,
    pub(super) answerer: Option<Answerer>,
}

pub(super) struct Waiting {
    key: String,
    state: Arc<SlotState>,
    rx: watch::Receiver<Option<Decision>>,
}

/// The dialog's end of its own slot. It cannot reach another one.
pub(super) struct Answerer {
    state: Arc<SlotState>,
}

/// Asked from inside the settings write, on the actor where uninstall runs too; the write either
/// ran before it (and its rows were cleared) or sees the abandonment.
pub(super) struct WriteGate {
    state: Arc<SlotState>,
}

impl Prompts {
    pub(super) fn enter(&self, module: &str, dependency: &str, load_id: u64) -> Entered {
        let key = format!("{module}::{dependency}");
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = slots.get(&key) {
            if slot.load_id == load_id && slot.state.current() != Some(Decision::Abandoned) {
                return Entered {
                    waiting: Waiting::on(key, slot.state.clone()),
                    answerer: None,
                };
            }
            // Another load owns the key, or the slot was abandoned: not a dialog to share.
            slot.state.abandon();
        }
        let state = SlotState::new();
        slots.insert(
            key.clone(),
            Slot {
                load_id,
                state: state.clone(),
            },
        );
        Entered {
            waiting: Waiting::on(key, state.clone()),
            answerer: Some(Answerer { state }),
        }
    }

    /// Abandon every slot whose key `owns` accepts, and forget it.
    pub(super) fn abandon_where(&self, mut owns: impl FnMut(&str) -> bool) {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots.retain(|key, slot| {
            if owns(key) {
                slot.state.abandon();
                false
            } else {
                true
            }
        });
    }

    /// Forget the slot a waiter settled on, unless a newer one has taken the key.
    pub(super) fn finish(&self, waiting: &Waiting) {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if slots
            .get(&waiting.key)
            .is_some_and(|slot| Arc::ptr_eq(&slot.state, &waiting.state))
        {
            slots.remove(&waiting.key);
        }
    }
}

impl Waiting {
    fn on(key: String, state: Arc<SlotState>) -> Self {
        let rx = state.tx.subscribe();
        Self { key, state, rx }
    }

    pub(super) async fn decide(&mut self) -> Decision {
        loop {
            if let Some(decision) = self.state.current() {
                return decision;
            }
            if self.rx.changed().await.is_err() {
                return Decision::Abandoned;
            }
        }
    }

    pub(super) fn gate(&self) -> WriteGate {
        WriteGate {
            state: self.state.clone(),
        }
    }
}

impl Answerer {
    pub(super) fn answer(self, answers: Vec<bool>) {
        self.state.settle(Decision::Answered(answers.into()));
    }

    /// The dialog ended without an answer (closed, or its window never came up): not a refusal.
    pub(super) fn withdraw(self) {
        self.state.settle(Decision::Abandoned);
    }
}

impl WriteGate {
    pub(super) fn is_open(&self) -> bool {
        self.state.current() != Some(Decision::Abandoned)
    }
}

/// What a decision leaves in the trust table: the user's answers, or nothing.
pub(super) fn recordable(decision: Decision) -> Option<Arc<[bool]>> {
    match decision {
        Decision::Answered(answers) => Some(answers),
        Decision::Abandoned => None,
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/trust_prompt.rs"]
mod tests;
