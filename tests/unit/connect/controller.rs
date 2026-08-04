//! Tests for `src/connect/controller/mod.rs`, attached to it by `#[path]`.

use super::*;
use crate::connect::types::DeviceType;

fn dev(fullname: &str) -> MdnsDevice {
    MdnsDevice {
        addresses: vec!["127.0.0.1".into()],
        friendly_name: "n".into(),
        fullname: fullname.into(),
        id: "id".into(),
        port: 1,
        device_type: DeviceType::TidalConnect,
    }
}

/// Retiring the session is the renderer's only notice of it, the device's own
/// `notifySessionEnded` arriving on a socket whose loop this same call just made stale.
#[tokio::test]
async fn disconnecting_a_live_session_returns_the_event_that_retires_it() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    let port: u16 = server.url().rsplit(':').next().unwrap().parse().unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);
    let dialing = tokio::spawn(async move {
        WsClient::connect("127.0.0.1", port, true, event_tx)
            .await
            .unwrap()
    });
    let _conn = server.accept().await.expect("accept");
    let ws = Arc::new(dialing.await.unwrap());

    let mut ctrl = TidalConnectController::new(None);
    ctrl.session = Some(ControllerSession::new(ws));
    let before = ctrl.connection_gen;

    let event = ctrl
        .disconnect(true)
        .expect("retiring a session has to announce it");

    assert!(matches!(
        event,
        ControllerSessionEvent::SessionEnded { suspended: false }
    ));
    assert!(ctrl.session.is_none(), "the session goes with the event");
    assert_ne!(
        ctrl.connection_gen, before,
        "and its event loop has to go stale"
    );

    // Why the announcement has to come from here at all: the device's own reply lands on a
    // controller with no session left to match it against, and yields nothing to emit. Anyone
    // moving the announcement back onto the acknowledgement has to break this first.
    let reply = WsClientEvent::Message(serde_json::json!({
        "command": "notifySessionEnded",
        "sessionId": "s1",
        "suspended": false,
    }));
    assert!(
        ctrl.handle_ws_event(&reply).is_none(),
        "the acknowledgement cannot carry the announcement"
    );
}

#[test]
fn disconnecting_without_a_session_announces_nothing() {
    let mut ctrl = TidalConnectController::new(None);

    // Nothing was ever connected, so there is no disconnection to report.
    assert!(ctrl.disconnect(true).is_none());
    // Keeping the cast alive touches neither the session nor the generation, so a genuine
    // notifySessionEnded still reaches the loop and is emitted from there instead.
    let before = ctrl.connection_gen;
    assert!(ctrl.disconnect(false).is_none());
    assert_eq!(ctrl.connection_gen, before);
}

#[test]
fn insert_device_capped_dedups_and_bounds() {
    let mut devices = Vec::new();
    insert_device_capped(&mut devices, dev("a"), 2);
    insert_device_capped(&mut devices, dev("b"), 2);
    // Dedup by fullname: re-seeing a known device does not grow the list.
    insert_device_capped(&mut devices, dev("a"), 2);
    assert_eq!(devices.len(), 2);
    // At the cap, a new distinct device is refused.
    insert_device_capped(&mut devices, dev("c"), 2);
    assert_eq!(devices.len(), 2);
    assert!(devices.iter().all(|d| d.fullname != "c"));
}
