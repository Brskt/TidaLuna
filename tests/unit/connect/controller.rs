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
    ctrl.start_session(&dev("d"), None).expect("start sent");
    let started = device_message(
        "notifySessionStarted",
        serde_json::json!({ "joined": false }),
    );
    assert!(ctrl.handle_ws_event(&started).is_some());
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

/// TIDAL's connect saga waits for the end of the device it holds before dialling another. A loss
/// tells it only CONNECTION_LOST, leaving that end owed (or the next pick hangs silently); one the
/// device sent, or no connection announced, owes none: a second wipes the persisted device and reloads the track.
#[tokio::test]
async fn disconnecting_announces_the_end_only_when_one_is_owed() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    let ended = |ctrl: &mut TidalConnectController| {
        let before = ctrl.connection_gen;
        let event = ctrl.disconnect(true);
        assert!(ctrl.session.is_none(), "the session goes with the call");
        assert_ne!(
            ctrl.connection_gen, before,
            "stamped past what is already stale"
        );
        matches!(
            event,
            Some(ControllerSessionEvent::SessionEnded { suspended: false })
        )
    };

    for (lapse, event, owed) in [
        ("live", None, true),
        ("lost", Some(WsClientEvent::ConnectionLost), true),
        (
            "ended by the device",
            Some(device_message(
                "notifySessionEnded",
                serde_json::json!({ "suspended": false }),
            )),
            false,
        ),
    ] {
        let (ws, _conn) = dial(&mut server).await;
        let mut ctrl = TidalConnectController::new(None);
        ctrl.session = Some(ControllerSession::new(ws));
        ctrl.start_session(&dev("d"), None).expect("start sent");
        let started = device_message(
            "notifySessionStarted",
            serde_json::json!({ "joined": false }),
        );
        assert!(ctrl.handle_ws_event(&started).is_some());
        assert!(ctrl.handle_ws_event(&lapse).is_some());
        assert_eq!(ended(&mut ctrl), owed, "an end owed: {owed}");
        assert!(!ended(&mut ctrl), "an end is announced once");
    }

    let mut idle = TidalConnectController::new(None);
    assert!(
        idle.disconnect(false).is_none(),
        "a plain close announces nothing"
    );
    assert!(!ended(&mut idle), "nothing was ever announced");
}

/// TIDAL's queue restore waits for a session or a failed connect, and a lost connection is neither:
/// a socket that dies before the device answers startSession has to read as a failed start, and
/// one that dies later as a plain disconnect.
#[tokio::test]
async fn a_socket_lost_reads_as_a_failed_start_before_the_session_and_a_disconnect_after() {
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
    let lost = WsClientEvent::ConnectionLost;

    ctrl.start_session(&dev("d"), None).expect("start sent");
    assert!(matches!(
        ctrl.handle_ws_event(&lost),
        Some(ControllerSessionEvent::ConnectionLost { during_start: true })
    ));

    ctrl.start_session(&dev("d"), None).expect("start sent");
    let started = WsClientEvent::Message(serde_json::json!({
        "command": "notifySessionStarted",
        "sessionId": "s1",
        "joined": false,
    }));
    assert!(ctrl.handle_ws_event(&started).is_some());
    assert!(matches!(
        ctrl.handle_ws_event(&lost),
        Some(ControllerSessionEvent::ConnectionLost {
            during_start: false
        })
    ));
}

/// A socket dialled to `server`, kept open by the connection it returns.
async fn dial(
    server: &mut crate::connect::testing::MockWsServer,
) -> (Arc<WsClient>, crate::connect::testing::MockWsConnection) {
    let port: u16 = server.url().rsplit(':').next().unwrap().parse().unwrap();
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);
    let dialing = tokio::spawn(async move {
        WsClient::connect("127.0.0.1", port, true, event_tx)
            .await
            .unwrap()
    });
    let conn = server.accept().await.expect("accept");
    (Arc::new(dialing.await.unwrap()), conn)
}

fn device_message(command: &str, extra: serde_json::Value) -> WsClientEvent {
    let mut json = serde_json::json!({ "command": command, "sessionId": "s1" });
    json.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    WsClientEvent::Message(json)
}

/// A controller whose session "s1" TIDAL was told started, then lost (`ended_suspended` false:
/// the connection dropped) or suspended by the device, with the resume sent on a new socket.
async fn resuming(
    ended_suspended: bool,
    server: &mut crate::connect::testing::MockWsServer,
) -> TidalConnectController {
    let (ws, _first) = dial(server).await;
    let mut ctrl = TidalConnectController::new(None);
    ctrl.session = Some(ControllerSession::new(ws));
    ctrl.start_session(&dev("d"), None).expect("start sent");
    let started = device_message(
        "notifySessionStarted",
        serde_json::json!({ "joined": false }),
    );
    assert!(ctrl.handle_ws_event(&started).is_some());
    assert!(ctrl.is_connected());

    let lapse = if ended_suspended {
        device_message(
            "notifySessionEnded",
            serde_json::json!({ "suspended": true }),
        )
    } else {
        WsClientEvent::ConnectionLost
    };
    assert!(ctrl.handle_ws_event(&lapse).is_some());
    assert!(!ctrl.is_connected(), "TIDAL was told the session is gone");

    let (ws, _second) = dial(server).await;
    let installed = ctrl.set_ws_and_start_session(ws, &dev("d"), None);
    assert!(installed.superseded.is_none(), "nothing live to replace");
    assert!(matches!(
        ctrl.start_session(&dev("d"), None),
        Some(SessionRequest::Resume)
    ));
    ctrl
}

/// TIDAL's `isConnected` turns true on DEVICE_CONNECTED alone, which the web app dispatches for
/// TIDAL Connect from the native `TCnotifySessionStarted` and from nothing else: a resume the
/// device accepts has to be reported as a start (once), or TIDAL stays disconnected from a
/// session Rust holds live.
#[tokio::test]
async fn a_resumed_session_is_announced_as_started_once() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    for ended_suspended in [false, true] {
        let mut ctrl = resuming(ended_suspended, &mut server).await;
        let failure = dial::ConnectFailure::Unresolved("d".into());
        assert!(
            ctrl.announces_connect_failure(&failure),
            "until the device answers, TIDAL is not connected"
        );

        let resumed = device_message("notifySessionResumed", serde_json::json!({}));
        let Some(ControllerSessionEvent::SessionStarted {
            session_id, joined, ..
        }) = ctrl.handle_ws_event(&resumed)
        else {
            panic!("a resume the device accepted is a connection TIDAL has to be told about");
        };
        assert_eq!((session_id.as_str(), joined), ("s1", true));
        assert!(ctrl.is_connected());
        assert!(!ctrl.announces_connect_failure(&failure));
        assert!(ctrl.session_info().is_some(), "a reloaded page is told too");

        assert!(
            ctrl.handle_ws_event(&resumed).is_none(),
            "TIDAL is connected already: a second DEVICE_CONNECTED would stack its listeners"
        );
    }
}

/// A refused resume is a connect that failed: the device keeps no such session, so keeping ours
/// live would leave TIDAL (told nothing connected) and Rust disagreeing for good.
#[tokio::test]
async fn a_refused_resume_retires_the_session_and_fails_the_connect() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    for ended_suspended in [false, true] {
        let mut ctrl = resuming(ended_suspended, &mut server).await;
        let refused = device_message(
            "notifySessionError",
            serde_json::json!({ "requestedCommand": "resumeSession", "details": {} }),
        );
        let event = ctrl
            .handle_ws_event(&refused)
            .expect("the refusal is reported");
        assert!(event.is_refused_connect());
        assert!(
            ctrl.session.is_none(),
            "the session is not resumable any more"
        );
        assert!(!ctrl.is_connected());
    }
}

/// A connect that replaces a live session closes its socket without the device saying anything:
/// TIDAL has to be told it ended, or it keeps `isConnected` for a session nothing reads.
#[tokio::test]
async fn replacing_a_live_session_tells_tidal_it_ended() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    for (replacement, suspended) in [("id", true), ("other", false)] {
        let (ws, _first) = dial(&mut server).await;
        let mut ctrl = TidalConnectController::new(None);
        ctrl.session = Some(ControllerSession::new(ws));
        ctrl.start_session(&dev("d"), None).expect("start sent");
        let started = device_message(
            "notifySessionStarted",
            serde_json::json!({ "joined": false }),
        );
        assert!(ctrl.handle_ws_event(&started).is_some());

        let mut target = dev("d");
        target.id = replacement.into();
        let (ws, _second) = dial(&mut server).await;
        let installed = ctrl.set_ws_and_start_session(ws, &target, None);
        assert!(
            matches!(
                installed.superseded,
                Some(ControllerSessionEvent::SessionEnded { suspended: s }) if s == suspended
            ),
            "replacement {replacement}"
        );
        assert!(!ctrl.is_connected());
    }
}

/// An install that does not complete takes its own socket down and no later one's.
#[tokio::test]
async fn an_abandoned_install_retires_only_its_own_socket() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    let (ws, _conn) = dial(&mut server).await;
    let mut ctrl = TidalConnectController::new(None);
    let installed = ctrl.set_ws_and_start_session(ws.clone(), &dev("d"), None);

    ctrl.start_session(&dev("d"), None).expect("start sent");
    let started = device_message(
        "notifySessionStarted",
        serde_json::json!({ "joined": false }),
    );
    assert!(ctrl.handle_ws_event(&started).is_some());

    assert!(
        ctrl.abandon_install(installed.conn_gen.wrapping_sub(1))
            .is_none()
    );
    assert!(
        ctrl.session.is_some() && ws.is_connected(),
        "a stale guard leaves the newer install"
    );

    assert!(matches!(
        ctrl.abandon_install(installed.conn_gen),
        Some(ControllerSessionEvent::SessionEnded { suspended: false })
    ));
    assert!(ctrl.session.is_none() && !ws.is_connected());
}

#[test]
fn insert_device_capped_dedups_and_bounds() {
    let mut devices = Vec::new();
    insert_device_capped(&mut devices, dev("a"), 2);
    // Dedup by fullname, below the cap: re-seeing a known device does not grow the list.
    insert_device_capped(&mut devices, dev("a"), 2);
    assert_eq!(devices.len(), 1);
    insert_device_capped(&mut devices, dev("b"), 2);
    assert_eq!(devices.len(), 2);
    // At the cap, a new distinct device is refused.
    insert_device_capped(&mut devices, dev("c"), 2);
    assert_eq!(devices.len(), 2);
    assert!(devices.iter().all(|d| d.fullname != "c"));
}
