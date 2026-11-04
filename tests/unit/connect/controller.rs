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

/// A connect that replaces a live session with another device's closes its socket without the
/// device saying anything: TIDAL has to be told it ended, or it keeps `isConnected` for a session
/// nothing reads. Devices are told apart by `fullname`: a TXT `id` can be empty or shared.
#[tokio::test]
async fn replacing_a_live_session_with_another_device_tells_tidal_it_ended() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    for replacement_id in ["id", "other"] {
        let (mut ctrl, _first) = live_on("id", &mut server).await;

        let mut target = dev("other");
        target.id = replacement_id.into();
        let (ws, _second) = dial(&mut server).await;
        let installed = ctrl.set_ws_and_start_session(ws, &target, None);
        assert!(
            matches!(
                installed.superseded,
                Some(ControllerSessionEvent::SessionEnded { suspended: false })
            ),
            "replacement with id {replacement_id}"
        );
        assert!(!ctrl.is_connected());
    }
}

/// TIDAL reconnects the device it casts to on every login, and its own controller does that
/// unseen: the session moves to the socket the device resumed it on, nothing sent to the page. An
/// end or a start announced here would stack a second cast listener in TIDAL ("next" skips two).
#[tokio::test]
async fn a_live_session_moves_to_its_resumed_socket_unseen() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    let (mut ctrl, _first) = live_on("id", &mut server).await;
    let old = ctrl.ws_client().unwrap();
    let plan = ctrl.resume_plan("d").expect("a live session has a plan");
    let before = ctrl.connection_gen;

    let (ws, _second) = dial(&mut server).await;
    let conn_gen = ctrl
        .adopt_resumed(ws.clone(), &dev("d"), &plan)
        .expect("still the planned session");
    assert_ne!(ctrl.connection_gen, before);
    assert!(
        !old.is_connected(),
        "the old socket closes once the new holds the session"
    );
    assert!(Arc::ptr_eq(&ctrl.ws_client().unwrap(), &ws));
    assert!(ctrl.is_connected(), "the session stays live for the page");
    let (_, _, _, connection) = ctrl.session_info().expect("still live");
    assert_eq!(connection, conn_gen);
}

/// A connection lost while the device confirmed the resume told TIDAL the cast was gone: the
/// resume notice replayed on the new socket is what tells it the cast is back.
#[tokio::test]
async fn a_session_lost_during_the_resume_is_announced_when_the_notice_is_replayed() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    let (mut ctrl, _first) = live_on("id", &mut server).await;
    let plan = ctrl.resume_plan("d").expect("a live session has a plan");
    assert!(
        ctrl.handle_ws_event(&WsClientEvent::ConnectionLost)
            .is_some()
    );

    let (ws, _second) = dial(&mut server).await;
    ctrl.adopt_resumed(ws, &dev("d"), &plan)
        .expect("the device resumed that very session");
    assert!(!ctrl.is_connected(), "nothing is announced by the install");

    let resumed = device_message("notifySessionResumed", serde_json::json!({}));
    assert!(matches!(
        ctrl.handle_ws_event(&resumed),
        Some(ControllerSessionEvent::SessionStarted { joined: true, .. })
    ));
}

/// The device ends the session or another device takes the cast while the resume is confirmed:
/// the socket the device resumed it on installs nothing. A new session there would be a start
/// nothing asked for.
#[tokio::test]
async fn a_resume_whose_session_is_gone_installs_nothing() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    type Intervene = fn(&mut TidalConnectController, Arc<WsClient>);
    let cases: [(&str, Intervene); 2] = [
        ("the device ends it", |ctrl, _| {
            let ended = device_message(
                "notifySessionEnded",
                serde_json::json!({ "suspended": false }),
            );
            assert!(ctrl.handle_ws_event(&ended).is_some());
        }),
        ("another device takes over", |ctrl, ws| {
            let _ = ctrl.set_ws_and_start_session(ws, &dev("other"), None);
        }),
    ];
    for (what, intervene) in cases {
        let (mut ctrl, _first) = live_on("id", &mut server).await;
        let plan = ctrl.resume_plan("d").expect("a live session has a plan");
        let (taker, _taker_conn) = dial(&mut server).await;
        intervene(&mut ctrl, taker.clone());
        let installed = ctrl.ws_client();

        let (ws, _second) = dial(&mut server).await;
        assert!(
            ctrl.adopt_resumed(ws.clone(), &dev("d"), &plan).is_none(),
            "{what}"
        );
        assert!(
            !ws.is_connected(),
            "{what}: the socket nothing took is closed"
        );
        assert_eq!(
            installed.map(|w| Arc::as_ptr(&w)),
            ctrl.ws_client().map(|w| Arc::as_ptr(&w)),
            "{what}: the controller keeps what it had"
        );
    }
}

/// A resume the device did not confirm leaves the live session on its old socket. When that socket
/// shut with its loss still to come, nothing is left to resume: the session ends the way a
/// disconnect ends it, with the end TIDAL is owed.
#[tokio::test]
async fn a_failed_resume_ends_the_session_only_when_its_old_socket_is_gone() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    let (mut ctrl, _first) = live_on("id", &mut server).await;
    assert!(ctrl.end_unless_live().is_none());
    assert!(ctrl.is_connected(), "a live session is left as it was");

    ctrl.ws_client().unwrap().shutdown();
    let before = ctrl.connection_gen;
    assert!(matches!(
        ctrl.end_unless_live(),
        Some(ControllerSessionEvent::SessionEnded { suspended: false })
    ));
    assert!(ctrl.session.is_none());
    assert_ne!(ctrl.connection_gen, before, "its late loss is stale");
    assert!(ctrl.end_unless_live().is_none(), "an end is announced once");
}

/// A controller with a session on device `id` that TIDAL was told started, its socket kept open
/// by the connection returned with it.
async fn live_on(
    id: &str,
    server: &mut crate::connect::testing::MockWsServer,
) -> (
    TidalConnectController,
    crate::connect::testing::MockWsConnection,
) {
    let (ws, conn) = dial(server).await;
    let mut ctrl = TidalConnectController::new(None);
    ctrl.session = Some(ControllerSession::new(ws));
    let mut device = dev("d");
    device.id = id.into();
    ctrl.start_session(&device, None).expect("start sent");
    let started = device_message(
        "notifySessionStarted",
        serde_json::json!({ "joined": false }),
    );
    assert!(ctrl.handle_ws_event(&started).is_some());
    (ctrl, conn)
}

fn update_for(token: &str) -> serde_json::Value {
    serde_json::json!({ "command": "updateServerInfo", "token": token })
}

/// A device that reports its server info failing has to be handed the token again even when it
/// is the one it holds: TIDAL's recovery pushes the same token, and the device retries on it.
#[tokio::test]
async fn a_device_error_owes_the_token_again() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    let (mut ctrl, _conn) = live_on("id", &mut server).await;

    assert!(matches!(
        ctrl.deliver_server_info("a", None, update_for("a")),
        ServerInfoClaim::Queued(_)
    ));
    assert!(matches!(
        ctrl.deliver_server_info("a", None, update_for("a")),
        ServerInfoClaim::Held
    ));

    for command in ["notifyContentServerError", "notifyQueueServerError"] {
        let error = device_message(command, serde_json::json!({ "errorCode": 401 }));
        assert!(ctrl.handle_ws_event(&error).is_none());
        assert!(
            matches!(
                ctrl.deliver_server_info("a", None, update_for("a")),
                ServerInfoClaim::Queued(_)
            ),
            "{command}"
        );
        assert!(matches!(
            ctrl.deliver_server_info("a", None, update_for("a")),
            ServerInfoClaim::Held
        ));
    }

    // Nothing is owed to a device holding no session: a lost connection has nothing to read it.
    assert!(
        ctrl.handle_ws_event(&WsClientEvent::ConnectionLost)
            .is_some()
    );
    assert!(matches!(
        ctrl.deliver_server_info("a", None, update_for("a")),
        ServerInfoClaim::NoSession
    ));
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
