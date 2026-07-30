//! Tests for `src/connect/mdns/backend.rs`, attached to it by `#[path]`.
//!
//! The `#[cfg(test)]` fakes they drive stay in the source file: they are types in
//! that module's namespace, not tests.

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
    // Drive it all the way to Shutdown before probing: a fixed sleep races
    // the daemon's own OS thread and flakes under parallel test load, so
    // wait on the status channel until the daemon actually reports down.
    let status = daemon.shutdown().expect("shutdown daemon");
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Ok(s) = status.recv_async().await {
            if matches!(s, DaemonStatus::Shutdown) {
                break;
            }
        }
    })
    .await
    .expect("daemon did not report shutdown within 5s");

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
