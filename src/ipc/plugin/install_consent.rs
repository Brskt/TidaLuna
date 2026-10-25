//! Who may ask the user to install a plugin: one dialog at a time.

use std::fmt;
use std::sync::Mutex;

#[derive(Default)]
pub(super) struct Consents {
    asking: Mutex<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    /// Another install is waiting for its answer.
    Busy,
    Declined,
    /// The dialog went away without an answer.
    Unanswered,
}

impl Refusal {
    pub(super) fn code(self) -> i32 {
        match self {
            Self::Busy | Self::Unanswered => 409,
            Self::Declined => 403,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Busy => "another plugin install is waiting for an answer",
            Self::Declined => "install declined",
            Self::Unanswered => "the install dialog closed without an answer",
        })
    }
}

/// The right to ask, held from before the fetch until the answer: dropping it on any path lets the
/// next install ask.
pub(super) struct Turn<'a> {
    consents: &'a Consents,
}

impl Consents {
    pub(super) fn begin(&self) -> Result<Turn<'_>, Refusal> {
        let mut asking = self.asking.lock().unwrap_or_else(|e| e.into_inner());
        if *asking {
            return Err(Refusal::Busy);
        }
        *asking = true;
        Ok(Turn { consents: self })
    }
}

impl Turn<'_> {
    /// `answer` is the dialog's: `None` when it ended without one.
    pub(super) fn settle(self, answer: Option<bool>) -> Result<(), Refusal> {
        match answer {
            Some(true) => Ok(()),
            Some(false) => Err(Refusal::Declined),
            None => Err(Refusal::Unanswered),
        }
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        *self
            .consents
            .asking
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = false;
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/install_consent.rs"]
mod tests;
