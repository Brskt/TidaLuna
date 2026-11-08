//! The disk reads a dialog's plugin-chosen path needs, kept off the UI and runtime threads. A dead
//! network mount holds such a read until its server returns: it runs on a thread of its own, one at
//! a time, and is given up on after a deadline, the dialog then opening without the path.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::util::one_at_a_time::OneAtATime;

/// A mount that answers reads in far less; one that misses it only costs the dialog its start.
const DEADLINE: Duration = Duration::from_secs(2);

static DIALOG_BUSY: AtomicBool = AtomicBool::new(false);

pub(super) static DIALOG_PATHS: Probe = Probe::new(DEADLINE, &DIALOG_BUSY);

pub(crate) struct Probe {
    busy: &'static AtomicBool,
    deadline: Duration,
}

impl Probe {
    pub(crate) const fn new(deadline: Duration, busy: &'static AtomicBool) -> Self {
        Self { busy, deadline }
    }

    /// `read`'s answer, or `None` when it misses the deadline or an earlier read is still stuck:
    /// however often a plugin asks, one thread at most waits on a dead mount.
    pub(crate) async fn run<T: Send + 'static>(
        &self,
        read: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let Some(turn) = OneAtATime::try_acquire(self.busy) else {
            crate::vprintln!("[DIALOG] A path is still waiting on its disk, opening without it");
            return None;
        };
        let (tx, rx) = oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("path-probe".into())
            .spawn(move || {
                // Frees the probe when the read ends, by returning or by panicking.
                let _turn = turn;
                let _ = tx.send(read());
            });
        if let Err(e) = spawned {
            crate::verr!("[DIALOG] Could not start the path check: {e}");
            return None;
        }
        match tokio::time::timeout(self.deadline, rx).await {
            Ok(answer) => answer.ok(),
            Err(_) => {
                crate::verr!(
                    "[DIALOG] A path did not answer within {:?}, opening without it",
                    self.deadline
                );
                None
            }
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/path_probe.rs"]
mod tests;
