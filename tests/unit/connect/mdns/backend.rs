//! Tests for `src/connect/mdns/backend.rs`, attached to it by `#[path]`.

use super::*;

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
