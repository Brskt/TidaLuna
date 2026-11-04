//! Once-per-outage reporting for the Connect operations that keep failing.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

/// Report state of an operation retried while it keeps failing (TIDAL's device picker
/// re-dispatches DISCOVER_DEVICES every 7 s). The first failure of an outage is a `verr!`; repeats
/// stay gated so they cannot spend the shared budget; a success re-arms.
#[derive(Default)]
pub(crate) struct FailureLatch {
    reported: AtomicBool,
}

impl FailureLatch {
    pub(crate) const fn new() -> Self {
        Self {
            reported: AtomicBool::new(false),
        }
    }

    pub(crate) fn failed(&self, args: fmt::Arguments<'_>) {
        if self.reported.swap(true, Ordering::Relaxed) {
            crate::logging::vlog(args);
        } else {
            crate::logging::vlog_err(args);
        }
    }

    pub(crate) fn recovered(&self) {
        self.reported.store(false, Ordering::Relaxed);
    }
}
