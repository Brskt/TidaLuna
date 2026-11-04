//! Tests for `src/connect/controller/dial.rs`, attached to it by `#[path]`.

use std::sync::Arc;

use super::*;
use crate::connect::mdns::browser::BrowserEvent;
use crate::connect::testing::MockWsServer;
use crate::connect::types::DeviceType;

/// Device `fullname`, announced at `address` and `port`.
fn device(fullname: &str, address: &str, port: u16) -> MdnsDevice {
    MdnsDevice {
        addresses: vec![address.into()],
        port,
        device_type: DeviceType::TidalConnect,
    }
}

fn port_of(server: &MockWsServer) -> u16 {
    server.url().rsplit(':').next().unwrap().parse().unwrap()
}

/// TIDAL resumes a cast before mDNS has answered, naming the device by key and carrying an
/// address of its own that a plugin can forge. The request has to wait for the resolution and
/// dial what Rust resolved.
#[tokio::test]
async fn a_request_waits_for_mdns_and_dials_what_it_resolved() {
    let mut server = MockWsServer::new().await.unwrap();
    let ctrl = Arc::new(Mutex::new(TidalConnectController::new(None)));
    let (events, _events_rx) = mpsc::channel(8);

    let requested = device("tidal", "203.0.113.9", 9);
    let dialling = tokio::spawn({
        let ctrl = ctrl.clone();
        async move { dial_requested(&ctrl, &requested, RESOLVE_DEADLINE, true, events).await }
    });
    tokio::task::yield_now().await;
    assert!(!dialling.is_finished(), "nothing resolved yet: it waits");

    ctrl.lock()
        .unwrap()
        .handle_browser_event(BrowserEvent::DeviceFound(device(
            "tidal",
            "127.0.0.1",
            port_of(&server),
        )));

    let dialled = dialling.await.unwrap().ok().expect("connected").1;
    assert_eq!(dialled.addresses, vec!["127.0.0.1".to_string()]);
    let _conn = server.accept().await.expect("the resolved address is dialled");
}

/// The request carries a reachable address for a device mDNS never found: it has to be refused,
/// not dialled on the strength of what the renderer said.
#[tokio::test]
async fn a_device_mdns_never_found_is_not_dialled() {
    let mut server = MockWsServer::new().await.unwrap();
    let ctrl = Mutex::new(TidalConnectController::new(None));
    let (events, _events_rx) = mpsc::channel(8);

    let requested = device("ghost", "127.0.0.1", port_of(&server));
    let outcome = dial_requested(&ctrl, &requested, Duration::from_millis(100), true, events).await;

    assert!(matches!(outcome, Err(ConnectFailure::Unresolved(_))));
    let accepted = tokio::time::timeout(Duration::from_millis(200), server.accept()).await;
    assert!(accepted.is_err(), "nothing may have dialled the server");
}
