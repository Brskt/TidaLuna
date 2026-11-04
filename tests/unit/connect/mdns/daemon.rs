//! Tests for `src/connect/mdns/daemon.rs`, attached to it by `#[path]`.

use std::time::{Duration, Instant};

use mdns_sd::DaemonStatus;

use super::OwnedDaemon;

#[test]
fn a_dropped_owner_stops_its_daemon_thread() {
    let owner = OwnedDaemon::new().unwrap();
    let handle = owner.share();
    drop(owner);

    // A leak is silent: the thread and its sockets stay with no error anywhere. The daemon
    // stops asynchronously, polled via its status until the 10 s deadline.
    let deadline = Instant::now() + Duration::from_secs(10);
    let stopped = loop {
        let shutdown = match handle.status() {
            Err(_) => true,
            Ok(rx) => matches!(
                rx.recv_timeout(Duration::from_millis(100)),
                Ok(DaemonStatus::Shutdown)
            ),
        };
        if shutdown || Instant::now() >= deadline {
            break shutdown;
        }
    };
    assert!(stopped, "the daemon outlived its owner");
}
