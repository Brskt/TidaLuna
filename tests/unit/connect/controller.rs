//! Tests for `src/connect/controller/mod.rs`, attached to it by `#[path]`.

use super::*;
use crate::connect::testing::device;

/// TIDAL's connect saga waits for the end of the device it holds before dialling another. A live
/// session owes that end, and so does a loss, which tells it only CONNECTION_LOST (or the next pick
/// hangs silently); one the device sent, or no connection announced, owes none: a second wipes the
/// persisted device and reloads the track. The device's own reply cannot carry it, landing on the
/// event loop this call makes stale.
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
        let (mut ctrl, _conn) = live_on("id", &mut server).await;
        if let Some(event) = &event {
            assert!(ctrl.handle_ws_event(event).is_some(), "{lapse}");
        }
        assert_eq!(ended(&mut ctrl), owed, "{lapse}: an end owed: {owed}");
        assert!(!ended(&mut ctrl), "{lapse}: an end is announced once");
    }

    let mut idle = TidalConnectController::new(None);
    assert!(
        idle.disconnect(false).is_none(),
        "a plain close announces nothing"
    );
    assert!(!ended(&mut idle), "nothing was ever announced");
}

/// A socket dialled to `server`, kept open by the connection it returns.
async fn dial(
    server: &mut crate::connect::testing::MockWsServer,
) -> (Arc<WsClient>, crate::connect::testing::MockWsConnection) {
    let port = server.port();
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

/// A start installed over a live session ends it in the page, then announces itself, or TIDAL
/// keeps `isConnected` for a session nothing reads.
#[tokio::test]
async fn a_start_installed_over_a_live_session_ends_it_in_the_page_first() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    let (mut ctrl, _first) = live_on("id", &mut server).await;

    let (ws, _second) = dial(&mut server).await;
    let adopted = ctrl.adopt_started(ws, &device("other"), "s2".into(), true);
    assert!(matches!(
        adopted.superseded,
        Some(ControllerSessionEvent::SessionEnded { suspended: false })
    ));
    let Some(ControllerSessionEvent::SessionStarted {
        session_id,
        joined,
        connection,
        ..
    }) = adopted.started
    else {
        panic!("the install announces the start");
    };
    assert_eq!(
        (session_id.as_str(), joined, connection),
        ("s2", true, adopted.conn_gen)
    );
    assert!(ctrl.is_connected());
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
    let adopted = ctrl
        .adopt_resumed(ws.clone(), &device("d"), &plan)
        .unwrap_or_else(|failure| panic!("{}", failure.message()));
    let conn_gen = adopted.conn_gen;
    assert!(adopted.started.is_none() && !adopted.token_owed);
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
            let _ = ctrl.adopt_started(ws, &device("other"), "s2".into(), false);
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
            matches!(
                ctrl.adopt_resumed(ws.clone(), &device("d"), &plan),
                Err(dial::ConnectFailure::Cancelled)
            ),
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

/// A controller whose session "s1" TIDAL was told started and then lost (`suspended` false) or
/// suspended by the device, the socket it held kept open by the connection returned.
async fn gone_on(
    suspended: bool,
    server: &mut crate::connect::testing::MockWsServer,
) -> (
    TidalConnectController,
    crate::connect::testing::MockWsConnection,
) {
    let (mut ctrl, conn) = live_on("id", server).await;
    let lapse = if suspended {
        device_message(
            "notifySessionEnded",
            serde_json::json!({ "suspended": true }),
        )
    } else {
        WsClientEvent::ConnectionLost
    };
    assert!(ctrl.handle_ws_event(&lapse).is_some());
    assert!(!ctrl.is_connected(), "TIDAL was told the session is gone");
    (ctrl, conn)
}

/// A session TIDAL was told lost or suspended is resumed through the same plan as a live one, so
/// that the device confirms it before anything moves; what the install announces is the start the
/// device's notice would have been, and the end it is owed stays owed from there. The loss comes
/// during the wait, after the plan was taken; the suspension, before.
#[tokio::test]
async fn a_gone_session_resumed_is_announced_by_the_install_and_owes_its_end() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    for suspended in [false, true] {
        let (mut ctrl, _first, plan) = if suspended {
            let (ctrl, conn) = gone_on(true, &mut server).await;
            let plan = ctrl
                .resume_plan("d")
                .expect("a gone session is resumed like a live one");
            (ctrl, conn, plan)
        } else {
            let (mut ctrl, conn) = live_on("id", &mut server).await;
            let plan = ctrl.resume_plan("d").expect("a live session has a plan");
            assert!(
                ctrl.handle_ws_event(&WsClientEvent::ConnectionLost)
                    .is_some()
            );
            (ctrl, conn, plan)
        };
        let failure = dial::ConnectFailure::Unresolved("d".into());
        assert!(
            ctrl.announces_connect_failure(&failure),
            "until the device answers, TIDAL is not connected"
        );

        let (ws, _second) = dial(&mut server).await;
        let adopted = ctrl
            .adopt_resumed(ws, &device("d"), &plan)
            .unwrap_or_else(|failure| panic!("{}", failure.message()));
        let Some(ControllerSessionEvent::SessionStarted {
            session_id,
            joined,
            connection,
            ..
        }) = adopted.started
        else {
            panic!("a session TIDAL was told gone is a connection it has to be told about");
        };
        assert_eq!(
            (session_id.as_str(), joined, connection),
            ("s1", true, adopted.conn_gen)
        );
        assert!(ctrl.is_connected());
        assert!(!ctrl.announces_connect_failure(&failure));
        assert!(ctrl.session_info().is_some(), "a reloaded page is told too");
        assert!(
            matches!(
                ctrl.disconnect(true),
                Some(ControllerSessionEvent::SessionEnded { suspended: false })
            ),
            "the start the install announced owes its end"
        );
    }
}

/// A token due while no live session can take it is owed to the install that follows, which hands
/// over the token then current.
#[tokio::test]
async fn a_token_due_while_no_session_is_live_is_owed_to_the_next_install() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    let (mut ctrl, _first) = gone_on(false, &mut server).await;
    assert!(matches!(
        ctrl.deliver_server_info("new", None, update_for("new")),
        ServerInfoClaim::NoSession
    ));
    let plan = ctrl.resume_plan("d").expect("a gone session has a plan");

    let (ws, mut second) = dial(&mut server).await;
    let adopted = ctrl
        .adopt_resumed(ws, &device("d"), &plan)
        .unwrap_or_else(|failure| panic!("{}", failure.message()));
    assert!(adopted.token_owed, "the install is told to hand it over");

    assert!(matches!(
        ctrl.deliver_server_info("newer", None, update_for("newer")),
        ServerInfoClaim::Queued(_)
    ));
    let sent = tokio::time::timeout(std::time::Duration::from_secs(2), second.recv())
        .await
        .expect("the adopted socket is the one handed it");
    assert_eq!(sent.expect("a frame")["token"], "newer");
}

/// A socket that died between the device's confirmation and the install is not traded for the
/// session's own: the resume fails, and a session whose old socket is gone then ends in the page.
#[tokio::test]
async fn a_socket_dead_at_the_install_leaves_the_session_as_it_was() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    for gone in [false, true] {
        let (mut ctrl, _first) = if gone {
            gone_on(false, &mut server).await
        } else {
            live_on("id", &mut server).await
        };
        let plan = ctrl.resume_plan("d").expect("a session has a plan");
        let old = ctrl.ws_client().unwrap();

        let (ws, _second) = dial(&mut server).await;
        ws.shutdown();
        assert!(
            matches!(
                ctrl.adopt_resumed(ws, &device("d"), &plan),
                Err(dial::ConnectFailure::Resume(_))
            ),
            "gone: {gone}"
        );
        assert!(
            Arc::ptr_eq(&ctrl.ws_client().unwrap(), &old),
            "gone: {gone}"
        );
        assert_eq!(ctrl.end_unless_live().is_some(), gone, "gone: {gone}");
    }
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
    let found = MdnsDevice {
        id: id.into(),
        ..device("d")
    };
    let _ = ctrl.adopt_started(ws, &found, "s1".into(), false);
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
}

/// An install that does not complete takes its own socket down and no later one's.
#[tokio::test]
async fn an_abandoned_install_retires_only_its_own_socket() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    let (ws, _conn) = dial(&mut server).await;
    let mut ctrl = TidalConnectController::new(None);
    let installed = ctrl.adopt_started(ws.clone(), &device("d"), "s1".into(), false);

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
    insert_device_capped(&mut devices, device("a"), 2);
    // Dedup by fullname, below the cap: re-seeing a known device does not grow the list.
    insert_device_capped(&mut devices, device("a"), 2);
    assert_eq!(devices.len(), 1);
    insert_device_capped(&mut devices, device("b"), 2);
    assert_eq!(devices.len(), 2);
    // At the cap, a new distinct device is refused.
    insert_device_capped(&mut devices, device("c"), 2);
    assert_eq!(devices.len(), 2);
    assert!(devices.iter().all(|d| d.fullname != "c"));
}

/// The device hears the end of a disconnect, then the close, and the socket ends with it: a device
/// that keeps the connection open (the official controller never closes its own) would otherwise
/// hold it for every session the user ends.
#[tokio::test]
async fn disconnecting_writes_the_end_before_closing_the_socket() {
    let mut server = crate::connect::testing::MockWsServer::new().await.unwrap();
    let (mut ctrl, mut conn) = live_on("id", &mut server).await;

    let _ = ctrl.disconnect(true);

    assert_eq!(
        conn.wire_until_end(std::time::Duration::from_secs(5)).await,
        vec!["endSession", "<close>", "<eof>"]
    );
}
