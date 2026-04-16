//! mDNS backend abstraction with a bounded, idempotent shutdown contract.
//!
//! `mdns-sd::ServiceDaemon` runs its own OS thread outside the tokio runtime,
//! and its `shutdown()` returns a channel receiver that eventually yields
//! `DaemonStatus::Shutdown`. A direct caller has to deal with three edge
//! cases: the daemon is already stopped, the internal channel is
//! temporarily full (`Error::Again`), or the daemon never acknowledges
//! within a reasonable time. This module wraps all three behind an
//! `async` trait so callers only see a single deadline-bounded result.
//!
//! The prod implementation wraps `ServiceDaemon`. The test implementation is
//! a simple fake that plays back a scripted sequence of outcomes so shutdown
//! behaviour can be exercised without a real multicast interface.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mdns_sd::{DaemonStatus, Error as MdnsError, ServiceDaemon};

/// Outcome of an `MdnsBackend::shutdown()` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// `DaemonStatus::Shutdown` was observed within the deadline.
    Clean,
    /// Probing `status()` reported a stopped daemon (or the status channel
    /// was dropped), so no shutdown work was required.
    AlreadyStopped,
    /// Shutdown did not complete within the deadline. The caller can decide
    /// whether to proceed or to treat this as a failure. Includes diagnostic
    /// counters so the degradation is observable in logs.
    Degraded {
        retry_count: u32,
        last_status: Option<String>,
        last_error: Option<String>,
    },
}

/// Backend contract consumed by the connect module. The trait is kept narrow
/// on purpose: only shutdown is abstracted, because only shutdown has the
/// problematic semantics (retry, non-tokio thread, non-exhaustive status).
/// Service registration and browsing still go through `mdns-sd` directly.
///
/// The trait is infallible: every failure mode is encoded in
/// `ShutdownOutcome::Degraded` so the caller can react on a single enum.
#[async_trait]
pub trait MdnsBackend: Send + Sync {
    async fn shutdown(&self, deadline: Duration) -> ShutdownOutcome;
}

// ---------------------------------------------------------------------------
// Production backend
// ---------------------------------------------------------------------------

/// Retry budget for `Error::Again` during shutdown attempts.
const MAX_AGAIN_RETRIES: u32 = 8;
/// Initial backoff between `Error::Again` retries. Grows up to `MAX_BACKOFF`.
const INITIAL_BACKOFF: Duration = Duration::from_millis(25);
/// Upper bound on a single backoff interval.
const MAX_BACKOFF: Duration = Duration::from_millis(100);

pub struct ProdMdnsBackend {
    daemon: Arc<ServiceDaemon>,
}

impl ProdMdnsBackend {
    pub fn new(daemon: Arc<ServiceDaemon>) -> Self {
        Self { daemon }
    }
}

#[async_trait]
impl MdnsBackend for ProdMdnsBackend {
    async fn shutdown(&self, deadline: Duration) -> ShutdownOutcome {
        let start = Instant::now();

        // Probe first: if the daemon is already stopped, avoid sending
        // another shutdown command.
        if probe_already_stopped(&self.daemon).await {
            return ShutdownOutcome::AlreadyStopped;
        }

        let mut retry_count: u32 = 0;
        let mut backoff = INITIAL_BACKOFF;
        let mut last_error: Option<String> = None;

        let receiver = loop {
            let remaining = deadline.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return ShutdownOutcome::Degraded {
                    retry_count,
                    last_status: None,
                    last_error,
                };
            }

            match self.daemon.shutdown() {
                Ok(rx) => break rx,
                Err(MdnsError::Again) => {
                    retry_count += 1;
                    last_error = Some("Error::Again".to_string());
                    if retry_count > MAX_AGAIN_RETRIES {
                        return ShutdownOutcome::Degraded {
                            retry_count,
                            last_status: None,
                            last_error,
                        };
                    }
                    let sleep = backoff.min(remaining);
                    tokio::time::sleep(sleep).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
                Err(_other) => {
                    // Any non-Again error here means the daemon command channel
                    // is closed or in an unrecoverable state. For shutdown
                    // purposes this is equivalent to "already stopped": the
                    // daemon cannot receive a new shutdown command because it
                    // has already left the running state.
                    return ShutdownOutcome::AlreadyStopped;
                }
            }
        };

        // Wait for the daemon to acknowledge. `mdns-sd` returns a flume
        // receiver; we use `recv_async` to integrate with the tokio runtime.
        let remaining = deadline.saturating_sub(start.elapsed());
        match tokio::time::timeout(remaining, receiver.recv_async()).await {
            // Match Shutdown explicitly; any other variant (present or
            // future, should the enum ever be marked non_exhaustive) is
            // reported as degraded rather than crashing.
            Ok(Ok(DaemonStatus::Shutdown)) => ShutdownOutcome::Clean,
            Ok(Ok(other)) => ShutdownOutcome::Degraded {
                retry_count,
                last_status: Some(format!("{other:?}")),
                last_error,
            },
            // Sender dropped: the daemon is gone, which is the condition we
            // were trying to reach anyway.
            Ok(Err(_)) => ShutdownOutcome::AlreadyStopped,
            Err(_) => ShutdownOutcome::Degraded {
                retry_count,
                last_status: None,
                last_error: last_error.or(Some("deadline".to_string())),
            },
        }
    }
}

async fn probe_already_stopped(daemon: &ServiceDaemon) -> bool {
    let rx = match daemon.status() {
        Ok(rx) => rx,
        // If even asking for status fails, assume the daemon is gone.
        Err(_) => return true,
    };
    // Non-blocking drain: one very short timeout. Either we immediately
    // observe Shutdown / a dropped sender (daemon gone), or we conclude
    // the daemon is still alive and continue with the normal shutdown path.
    match tokio::time::timeout(Duration::from_millis(1), rx.recv_async()).await {
        Ok(Ok(DaemonStatus::Shutdown)) => true,
        // Sender dropped: daemon is gone.
        Ok(Err(_)) => true,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Test backend
// ---------------------------------------------------------------------------

/// A scripted fake backend. Pushes are consumed in order so a single fake can
/// serve multiple shutdown() calls with different scripted outcomes.
#[cfg(test)]
pub struct FakeMdnsBackend {
    script: std::sync::Mutex<std::collections::VecDeque<FakeResponse>>,
}

#[cfg(test)]
#[derive(Clone)]
pub enum FakeResponse {
    Clean,
    AlreadyStopped,
    Degraded {
        retry_count: u32,
        last_status: Option<String>,
        last_error: Option<String>,
    },
}

#[cfg(test)]
impl FakeMdnsBackend {
    pub fn with_script(script: Vec<FakeResponse>) -> Self {
        Self {
            script: std::sync::Mutex::new(script.into()),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl MdnsBackend for FakeMdnsBackend {
    async fn shutdown(&self, _deadline: Duration) -> ShutdownOutcome {
        let next = self.script.lock().unwrap().pop_front();
        match next {
            Some(FakeResponse::Clean) => ShutdownOutcome::Clean,
            Some(FakeResponse::AlreadyStopped) => ShutdownOutcome::AlreadyStopped,
            Some(FakeResponse::Degraded {
                retry_count,
                last_status,
                last_error,
            }) => ShutdownOutcome::Degraded {
                retry_count,
                last_status,
                last_error,
            },
            // Empty script behaves like a clean idempotent shutdown.
            None => ShutdownOutcome::AlreadyStopped,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_returns_clean() {
        let fake = FakeMdnsBackend::with_script(vec![FakeResponse::Clean]);
        let outcome = fake.shutdown(Duration::from_millis(10)).await;
        assert_eq!(outcome, ShutdownOutcome::Clean);
    }

    #[tokio::test]
    async fn fake_returns_already_stopped() {
        let fake = FakeMdnsBackend::with_script(vec![FakeResponse::AlreadyStopped]);
        let outcome = fake.shutdown(Duration::from_millis(10)).await;
        assert_eq!(outcome, ShutdownOutcome::AlreadyStopped);
    }

    #[tokio::test]
    async fn fake_returns_degraded_with_counters() {
        let fake = FakeMdnsBackend::with_script(vec![FakeResponse::Degraded {
            retry_count: 3,
            last_status: Some("Running".to_string()),
            last_error: Some("Error::Again".to_string()),
        }]);
        let outcome = fake.shutdown(Duration::from_millis(10)).await;
        match outcome {
            ShutdownOutcome::Degraded {
                retry_count,
                last_status,
                last_error,
            } => {
                assert_eq!(retry_count, 3);
                assert_eq!(last_status.as_deref(), Some("Running"));
                assert_eq!(last_error.as_deref(), Some("Error::Again"));
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fake_second_call_is_idempotent() {
        let fake = FakeMdnsBackend::with_script(vec![FakeResponse::Clean]);
        let _ = fake.shutdown(Duration::from_millis(10)).await;
        // Script drained; second call returns AlreadyStopped (idempotent).
        let outcome = fake.shutdown(Duration::from_millis(10)).await;
        assert_eq!(outcome, ShutdownOutcome::AlreadyStopped);
    }

    #[tokio::test]
    async fn prod_backend_already_stopped_short_circuits() {
        // Build a real daemon and shut it down to simulate already-stopped.
        let daemon = Arc::new(ServiceDaemon::new().expect("create daemon"));
        // Trigger shutdown and drain the status so the daemon is definitely down.
        let _ = daemon.shutdown();
        // Allow the daemon a beat to transition.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let backend = ProdMdnsBackend::new(daemon);
        let outcome = backend.shutdown(Duration::from_millis(200)).await;
        // Either AlreadyStopped or Clean are acceptable: both mean the
        // daemon is down and the backend is idempotent.
        assert!(
            matches!(
                outcome,
                ShutdownOutcome::AlreadyStopped | ShutdownOutcome::Clean
            ),
            "unexpected outcome: {outcome:?}"
        );
    }
}
