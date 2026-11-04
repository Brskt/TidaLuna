//! Tests for `src/connect/controller/dial.rs`, attached to it by `#[path]`.

use std::sync::Arc;

use super::*;
use crate::connect::mdns::browser::BrowserEvent;
use crate::connect::testing::MockWsServer;

/// Device `fullname`, announced at `address` and `port`.
fn device(fullname: &str, address: &str, port: u16) -> MdnsDevice {
    MdnsDevice {
        addresses: vec![address.into()],
        port,
        ..crate::connect::testing::device(fullname)
    }
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
            server.port(),
        )));

    let dialled = dialling.await.unwrap().ok().expect("connected").1;
    assert_eq!(dialled.addresses, vec!["127.0.0.1".to_string()]);
    let _conn = server
        .accept()
        .await
        .expect("the resolved address is dialled");
}

/// The request carries a reachable address for a device mDNS never found: it has to be refused,
/// not dialled on the strength of what the renderer said.
#[tokio::test]
async fn a_device_mdns_never_found_is_not_dialled() {
    let mut server = MockWsServer::new().await.unwrap();
    let ctrl = Mutex::new(TidalConnectController::new(None));
    let (events, _events_rx) = mpsc::channel(8);

    let requested = device("ghost", "127.0.0.1", server.port());
    let outcome = dial_requested(&ctrl, &requested, Duration::from_millis(100), true, events).await;

    assert!(matches!(outcome, Err(ConnectFailure::Unresolved(_))));
    let accepted = tokio::time::timeout(Duration::from_millis(200), server.accept()).await;
    assert!(accepted.is_err(), "nothing may have dialled the server");
}

/// A socket dialled to `server` with the channel its events arrive on, and the device's side.
async fn dialled(
    server: &mut MockWsServer,
) -> (
    WsClient,
    mpsc::Receiver<WsClientEvent>,
    crate::connect::testing::MockWsConnection,
) {
    let port = server.port();
    let (events, events_rx) = mpsc::channel(64);
    let dialling =
        tokio::spawn(async move { WsClient::connect("127.0.0.1", port, true, events).await });
    let conn = server.accept().await.expect("accept");
    (dialling.await.unwrap().expect("dial"), events_rx, conn)
}

/// The device has to say it resumed the session before anything moves, and what it sent first
/// is not lost: the session still has to see those frames once it is on the new socket.
#[tokio::test]
async fn a_resume_waits_for_the_device_and_keeps_what_it_sent_first() {
    let mut server = MockWsServer::new().await.unwrap();
    let (ws, mut events, mut conn) = dialled(&mut server).await;

    let mut wait = Box::pin(confirm_resume(&ws, &mut events, "s1", RESUME_DEADLINE));
    assert!(
        futures_util::future::poll_immediate(&mut wait)
            .await
            .is_none(),
        "nothing is confirmed until the device says so"
    );
    let resume = conn.recv().await.expect("the resume is asked");
    assert_eq!(
        (resume["command"].as_str(), resume["sessionId"].as_str()),
        (Some("resumeSession"), Some("s1"))
    );

    conn.send(serde_json::json!({ "command": "notifyPlayerStatusChanged" }))
        .await;
    conn.send(serde_json::json!({ "command": "notifySessionResumed", "sessionId": "s1" }))
        .await;
    let Ok(frames) = wait.await else {
        panic!("the device confirmed");
    };
    let commands: Vec<_> = frames
        .iter()
        .map(|f| match f {
            WsClientEvent::Message(m) => m["command"].as_str().unwrap().to_owned(),
            _ => panic!("only frames are set aside"),
        })
        .collect();
    assert_eq!(
        commands,
        ["notifyPlayerStatusChanged", "notifySessionResumed"]
    );
}

/// A device that refuses the resume, or answers only for another session, has not resumed ours:
/// each fails the dial instead of moving the session onto a socket nothing reads. A refusal ends
/// the wait at once, its deadline far beyond the test's bound; the deadline ends the other one.
#[tokio::test]
async fn a_resume_the_device_did_not_confirm_fails() {
    let mut server = MockWsServer::new().await.unwrap();
    let answers = [
        (
            serde_json::json!({
                "command": "notifySessionError", "requestedCommand": "resumeSession", "details": {}
            }),
            Duration::from_secs(60),
        ),
        (
            serde_json::json!({ "command": "notifySessionResumed", "sessionId": "another" }),
            Duration::from_millis(200),
        ),
    ];
    for (answer, deadline) in answers {
        let (ws, mut events, mut conn) = dialled(&mut server).await;
        conn.send(answer.clone()).await;
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            confirm_resume(&ws, &mut events, "s1", deadline),
        )
        .await
        .unwrap_or_else(|_| panic!("{answer}: the wait outlived its bound"));
        assert!(
            matches!(outcome, Err(ConnectFailure::Resume(_))),
            "{answer}"
        );
    }
}
