//! When a decode stops: the page that asked for it has given up by then, and nothing past it is
//! read. Checked before each `decode()` call, the one unit of work that cannot be interrupted.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Deadline {
    at: Instant,
    limit: Duration,
}

impl Deadline {
    /// `limit` past `start`, the moment the request reached us.
    pub(crate) fn after(start: Instant, limit: Duration) -> Self {
        Self {
            at: start + limit,
            limit,
        }
    }

    /// An error once it has passed: never the clean end a cancel gets, which the page would take
    /// for the whole body.
    pub(crate) fn check(self) -> anyhow::Result<()> {
        if Instant::now() >= self.at {
            anyhow::bail!(
                "the decode ran past its deadline of {} s",
                self.limit.as_secs()
            );
        }
        Ok(())
    }
}
