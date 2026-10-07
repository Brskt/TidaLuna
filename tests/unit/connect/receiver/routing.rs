//! Tests for the receiver's routing task and its teardown in `src/connect/receiver/mod.rs`,
//! attached by `#[path]`. They drive the real `routing_loop` and `ConnectReceiver::shutdown`
//! over a server whose one client socket is a channel the test reads.

use super::*;
use std::time::Instant;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// A queue server that accepts, reads what it is sent (a TLS ClientHello) and never answers.
/// Reports each accepted connection, and the instant each one is closed by the client.
struct Stall {
    port: u16,
    accepted: std::sync::mpsc::Receiver<()>,
    closed: std::sync::mpsc::Receiver<Instant>,
}

fn stall_server() -> Stall {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (accepted_tx, accepted) = std::sync::mpsc::channel();
    let (closed_tx, closed) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let _ = accepted_tx.send(());
            let closed_tx = closed_tx.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match std::io::Read::read(&mut stream, &mut buf) {
                        Ok(0) | Err(_) => {
                            let _ = closed_tx.send(Instant::now());
                            break;
                        }
                        Ok(_) => {}
                    }
                }
            });
        }
    });
    Stall {
        port,
        accepted,
        closed,
    }
}

/// Every message written to the wire within `within`, and whether the socket was closed.
async fn wire_within(
    wire: &mut mpsc::Receiver<WsMessage>,
    within: Duration,
) -> (Vec<WsMessage>, bool) {
    let mut heard = Vec::new();
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, wire.recv()).await {
            Ok(Some(message)) => heard.push(message),
            Ok(None) => return (heard, true),
            Err(_) => return (heard, false),
        }
    }
}

/// A message's command, its request id appended when it carries one.
fn command_of(message: &serde_json::Value) -> String {
    let command = message["command"].as_str().unwrap_or("?");
    match message["requestId"].as_u64() {
        Some(rid) => format!("{command}#{rid}"),
        None => command.to_string(),
    }
}

/// Every command written to the wire within `within`, and whether the socket was closed. A frame
/// other than text is kept, by its debug form: no assertion on the commands lets one pass unseen.
async fn drain_wire(wire: &mut mpsc::Receiver<WsMessage>, within: Duration) -> (Vec<String>, bool) {
    let (heard, closed) = wire_within(wire, within).await;
    let commands = heard
        .into_iter()
        .map(|message| match message {
            WsMessage::Text(text) => command_of(&serde_json::from_str(&text).unwrap_or_default()),
            other => format!("{other:?}"),
        })
        .collect();
    (commands, closed)
}

/// Waits on a std channel without blocking the test's single runtime thread.
async fn recv_within<T>(rx: &std::sync::mpsc::Receiver<T>, within: Duration) -> Option<T> {
    let deadline = Instant::now() + within;
    loop {
        if let Ok(value) = rx.try_recv() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn load_cloud_queue(stall_port: u16, request_id: u64) -> serde_json::Value {
    serde_json::json!({
        "command": "loadCloudQueue",
        "requestId": request_id,
        "queueServerInfo": {
            "serverUrl": format!("https://api.tidal.com:{stall_port}/v1/queues"),
            "authInfo": { "headerAuth": "Bearer test" },
        },
        "queueInfo": {
            "queueId": "q-1",
            "repeatMode": "NONE",
            "shuffled": false,
            "maxAfterSize": 10,
            "maxBeforeSize": 10,
        },
    })
}

/// The receiver switched off while a phone's `loadCloudQueue` waits on a queue server that
/// never answers (the request is in its TLS handshake, under `HTTP_TIMEOUT_SECS`).
#[tokio::test]
async fn a_switch_off_during_a_stalled_queue_fetch_runs_the_epilogue_and_frees_the_port() {
    let _serialised = crate::connect::bridge::tests::BRIDGE_TESTS.lock().await;
    let stall = stall_server();
    let (server, listener, mut wire, filler) = WsServer::with_client_for_test(1, 64).await;
    drop(filler);
    let (client_tx, client_rx) = mpsc::channel::<ClientEvent>(128);
    let (mut receiver, _bridge_tx) =
        ConnectReceiver::launch(server, client_rx, stalled_http(&stall), None).unwrap();

    client_tx
        .send(ClientEvent::Message {
            socket_id: 1,
            message: serde_json::json!({ "command": "startSession" }),
        })
        .await
        .unwrap();
    let (before, _) = drain_wire(&mut wire, Duration::from_millis(500)).await;
    assert!(
        before.iter().any(|c| c == "notifySessionStarted"),
        "no session: {before:?}"
    );

    client_tx
        .send(ClientEvent::Message {
            socket_id: 1,
            message: load_cloud_queue(stall.port, 7),
        })
        .await
        .unwrap();
    recv_within(&stall.accepted, Duration::from_secs(5))
        .await
        .expect("the queue fetch never reached the queue server");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let started = Instant::now();
    let report = receiver.shutdown(SHUTDOWN_DEADLINE).await;
    let (after, closed) = drain_wire(&mut wire, Duration::from_secs(1)).await;
    let request_closed = recv_within(&stall.closed, Duration::from_secs(2))
        .await
        .map(|at| at.saturating_duration_since(started));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(listener.closed(), "the listener outlived the receiver");
    assert_eq!(
        report.graceful_completed,
        vec!["receiver-routing"],
        "the routing task did not reach its end: {report:?}"
    );
    assert!(
        after.iter().any(|c| c == "notifySessionEnded"),
        "the phone never heard the session end: {after:?}"
    );
    assert!(closed, "the phone's socket was never closed");
    assert!(
        request_closed.is_some(),
        "the stalled queue request outlived the receiver"
    );
}

/// The exit budget is shorter than the server's own flush (`SERVER_SHUTDOWN_DEADLINE` for a
/// stalled peer): the routing task is aborted inside its epilogue, in `WsServer::shutdown`.
#[tokio::test]
async fn an_abort_inside_the_epilogue_frees_the_port() {
    let _serialised = crate::connect::bridge::tests::BRIDGE_TESTS.lock().await;
    let (server, listener, wire_never_read, filler) = WsServer::with_client_for_test(1, 1).await;
    // The wire takes one message; the writer parks on the second one.
    filler.try_send(WsMessage::Ping(vec![].into())).unwrap();
    filler.try_send(WsMessage::Ping(vec![].into())).unwrap();
    let (_client_tx, client_rx) = mpsc::channel::<ClientEvent>(128);
    let (mut receiver, _bridge_tx) =
        ConnectReceiver::launch(server, client_rx, reqwest::Client::new(), None).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let report = receiver
        .shutdown(crate::connect::EXIT_SHUTDOWN_BUDGET)
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Read nothing: a read would free the parked writer, as a phone that resumed reading.
    let writer_released = wire_never_read.sender_strong_count() == 0;
    assert!(
        listener.closed(),
        "the listener outlived the receiver ({report:?})"
    );
    assert!(
        writer_released,
        "the parked writer outlived the receiver, and the phone's socket with it ({report:?})"
    );
}

/// The receiver around a routing task that panics, holding `server`. No panic site is known in
/// the loop: the stand-in panics where the loop would.
fn receiver_whose_routing_panics(server: WsServer) -> ConnectReceiver {
    let tasks = Arc::new(TaskGroup::new());
    tasks
        .spawn("receiver-routing", async move {
            let _client_comm = ClientCommunicator::new(server);
            tokio::task::yield_now().await;
            panic!("routing loop crash");
        })
        .unwrap();
    ConnectReceiver {
        tasks,
        advertiser: None,
    }
}

#[tokio::test]
async fn a_panic_in_the_routing_task_frees_the_port_and_the_phones() {
    let (server, listener, mut wire, filler) = WsServer::with_client_for_test(1, 64).await;
    drop(filler);
    let mut receiver = receiver_whose_routing_panics(server);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let report = receiver.shutdown(SHUTDOWN_DEADLINE).await;
    let (_, closed) = drain_wire(&mut wire, Duration::from_secs(1)).await;
    assert!(
        listener.closed(),
        "the listener outlived the crash ({report:?})"
    );
    assert!(closed, "the phone's socket outlived the crash ({report:?})");
}

/// The receiver switched off with a session open and nothing in flight.
#[tokio::test]
async fn a_switch_off_with_a_session_open_tells_the_phone_before_closing() {
    let _serialised = crate::connect::bridge::tests::BRIDGE_TESTS.lock().await;
    let (server, listener, mut wire, filler) = WsServer::with_client_for_test(1, 64).await;
    drop(filler);
    let (client_tx, client_rx) = mpsc::channel::<ClientEvent>(128);
    let (mut receiver, _bridge_tx) =
        ConnectReceiver::launch(server, client_rx, reqwest::Client::new(), None).unwrap();
    client_tx
        .send(ClientEvent::Message {
            socket_id: 1,
            message: serde_json::json!({ "command": "startSession" }),
        })
        .await
        .unwrap();
    let (before, _) = drain_wire(&mut wire, Duration::from_millis(500)).await;
    assert!(
        before.iter().any(|c| c == "notifySessionStarted"),
        "no session: {before:?}"
    );
    let report = receiver.shutdown(SHUTDOWN_DEADLINE).await;
    let (after, closed) = drain_wire(&mut wire, Duration::from_secs(1)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        listener.closed(),
        "the listener outlived the receiver ({report:?})"
    );
    assert_eq!(
        after,
        vec!["notifySessionEnded".to_string()],
        "what the phone heard last"
    );
    assert!(closed, "the phone's socket was never closed");
}

/// A panic in the routing task while a phone has stopped reading, its writer parked in a send.
#[tokio::test]
async fn a_panic_with_a_peer_that_stopped_reading_releases_its_socket() {
    let (server, listener, wire, filler) = WsServer::with_client_for_test(1, 1).await;
    filler.try_send(WsMessage::Ping(vec![].into())).unwrap();
    filler.try_send(WsMessage::Ping(vec![].into())).unwrap();
    drop(filler);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut receiver = receiver_whose_routing_panics(server);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let report = receiver.shutdown(SHUTDOWN_DEADLINE).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Read nothing: a read would free the parked writer, as a phone that resumed reading.
    let writer_released = wire.sender_strong_count() == 0;
    assert!(
        listener.closed(),
        "the listener outlived the crash ({report:?})"
    );
    assert!(
        writer_released,
        "the parked writer outlived the crash, and the phone's socket with it ({report:?})"
    );
}

/// A phone that starts a session and closes before the loop takes either: queued behind the
/// command, the disconnect leaves neither a client nor a live session holding the socket, and the
/// switch-off that follows has no session to end.
#[tokio::test]
async fn a_command_taken_before_its_sockets_disconnect_leaves_nothing_of_the_socket() {
    use crate::connect::bridge::tests::{BRIDGE_TESTS, has_client};
    let _serialised = BRIDGE_TESTS.lock().await;
    let (server, _listener, mut wire, filler) = WsServer::with_client_for_test(1, 64).await;
    drop(filler);
    let (client_tx, client_rx) = mpsc::channel::<ClientEvent>(128);
    for event in [
        ClientEvent::Connected(1),
        ClientEvent::Message {
            socket_id: 1,
            message: serde_json::json!({ "command": "startSession" }),
        },
        ClientEvent::Disconnected(1),
    ] {
        client_tx.send(event).await.unwrap();
    }
    let (mut receiver, _bridge_tx) =
        ConnectReceiver::launch(server, client_rx, reqwest::Client::new(), None).unwrap();

    let (before, _) = drain_wire(&mut wire, Duration::from_millis(500)).await;
    assert_eq!(before, vec!["notifySessionStarted".to_string()]);
    let deadline = Instant::now() + Duration::from_secs(2);
    while has_client() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!has_client(), "the closed socket still counts as a client");

    receiver.shutdown(SHUTDOWN_DEADLINE).await;
    let (after, closed) = drain_wire(&mut wire, Duration::from_secs(1)).await;
    assert!(after.is_empty(), "a session outlived its socket: {after:?}");
    assert!(closed, "the socket was never closed");
}

/// Every text message written to the wire within `within`, whole.
async fn heard_on(
    wire: &mut mpsc::Receiver<WsMessage>,
    within: Duration,
) -> Vec<serde_json::Value> {
    let (heard, _) = wire_within(wire, within).await;
    heard
        .into_iter()
        .filter_map(|message| match message {
            WsMessage::Text(text) => Some(serde_json::from_str(&text).unwrap_or_default()),
            _ => None,
        })
        .collect()
}

/// A queue fetch's client, sent to a queue server that never answers.
fn stalled_http(stall: &Stall) -> reqwest::Client {
    reqwest::Client::builder()
        .resolve("api.tidal.com", ([127, 0, 0, 1], stall.port).into())
        .build()
        .unwrap()
}

/// What a socket outside any session sends besides the session commands. The queue comes last:
/// its fetch never ends, and holds back whatever follows it.
fn an_outsiders_commands(stall_port: u16) -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({ "command": "next", "requestId": 8 }),
        serde_json::json!({ "command": "setVolume", "requestId": 9, "level": 1.0 }),
        serde_json::json!({ "command": "setMute", "requestId": 10, "mute": true }),
        serde_json::json!({ "command": "play", "requestId": 11 }),
        serde_json::json!({ "command": "requestResources", "requestId": 12 }),
        load_cloud_queue(stall_port, 7),
    ]
}

#[tokio::test]
async fn a_socket_with_no_session_is_answered_nothing_and_fetches_no_queue() {
    let _serialised = crate::connect::bridge::tests::BRIDGE_TESTS.lock().await;
    let stall = stall_server();
    let (server, _listener, mut wire, filler) = WsServer::with_client_for_test(1, 64).await;
    drop(filler);
    let (client_tx, client_rx) = mpsc::channel::<ClientEvent>(128);
    client_tx.send(ClientEvent::Connected(1)).await.unwrap();
    for message in an_outsiders_commands(stall.port) {
        client_tx
            .send(ClientEvent::Message {
                socket_id: 1,
                message,
            })
            .await
            .unwrap();
    }
    let (mut receiver, _bridge_tx) =
        ConnectReceiver::launch(server, client_rx, stalled_http(&stall), None).unwrap();

    let heard = heard_on(&mut wire, Duration::from_millis(500)).await;
    let fetched = recv_within(&stall.accepted, Duration::from_millis(200)).await;
    receiver.shutdown(SHUTDOWN_DEADLINE).await;
    assert!(heard.is_empty(), "the socket was answered: {heard:?}");
    assert!(fetched.is_none(), "the socket's queue was fetched");
}

/// Socket 2 holds the live session; socket 1, the one the test reads, never joined it.
#[tokio::test]
async fn a_socket_outside_another_phones_session_is_answered_nothing() {
    let _serialised = crate::connect::bridge::tests::BRIDGE_TESTS.lock().await;
    let stall = stall_server();
    let (server, _listener, mut wire, filler) = WsServer::with_client_for_test(1, 64).await;
    drop(filler);
    let (client_tx, client_rx) = mpsc::channel::<ClientEvent>(128);
    let mut events = vec![
        ClientEvent::Connected(1),
        ClientEvent::Connected(2),
        ClientEvent::Message {
            socket_id: 2,
            message: serde_json::json!({ "command": "startSession" }),
        },
    ];
    events.extend(
        an_outsiders_commands(stall.port)
            .into_iter()
            .map(|message| ClientEvent::Message {
                socket_id: 1,
                message,
            }),
    );
    for event in events {
        client_tx.send(event).await.unwrap();
    }
    let (mut receiver, _bridge_tx) =
        ConnectReceiver::launch(server, client_rx, stalled_http(&stall), None).unwrap();

    let heard = heard_on(&mut wire, Duration::from_millis(500)).await;
    let fetched = recv_within(&stall.accepted, Duration::from_millis(200)).await;
    receiver.shutdown(SHUTDOWN_DEADLINE).await;
    assert!(heard.is_empty(), "the outsider was answered: {heard:?}");
    assert!(fetched.is_none(), "the outsider's queue was fetched");
}

#[tokio::test]
async fn a_command_right_behind_a_start_session_is_the_new_members() {
    let _serialised = crate::connect::bridge::tests::BRIDGE_TESTS.lock().await;
    let stall = stall_server();
    let (server, _listener, mut wire, filler) = WsServer::with_client_for_test(1, 64).await;
    drop(filler);
    let (client_tx, client_rx) = mpsc::channel::<ClientEvent>(128);
    for event in [
        ClientEvent::Connected(1),
        ClientEvent::Message {
            socket_id: 1,
            message: serde_json::json!({ "command": "startSession" }),
        },
        ClientEvent::Message {
            socket_id: 1,
            message: serde_json::json!({ "command": "next", "requestId": 8 }),
        },
        ClientEvent::Message {
            socket_id: 1,
            message: load_cloud_queue(stall.port, 7),
        },
    ] {
        client_tx.send(event).await.unwrap();
    }
    let (mut receiver, _bridge_tx) =
        ConnectReceiver::launch(server, client_rx, stalled_http(&stall), None).unwrap();

    let heard = heard_on(&mut wire, Duration::from_millis(500)).await;
    let fetched = recv_within(&stall.accepted, Duration::from_secs(5)).await;
    receiver.shutdown(SHUTDOWN_DEADLINE).await;
    // The step is answered by the arm that took it. `notifySessionStarted` goes through another
    // arm, which the queue fetch that never ends can hold back: only the step's answer is read.
    let answers: Vec<String> = heard
        .iter()
        .filter(|m| m["command"] == "notifyRequestResult")
        .map(|m| format!("{}:{}", m["requestId"], m["resultCode"]))
        .collect();
    assert_eq!(
        answers,
        vec!["8:1002".to_string()],
        "the step is a member's, refused for want of a queue: {heard:?}"
    );
    assert!(fetched.is_some(), "the member's queue was never fetched");
}

/// A session suspended by its phone's own `endSession` admits nobody, its phone included, until
/// a `resumeSession` names it.
#[tokio::test]
async fn a_suspended_session_takes_its_phones_commands_again_once_resumed() {
    let _serialised = crate::connect::bridge::tests::BRIDGE_TESTS.lock().await;
    let (server, _listener, mut wire, filler) = WsServer::with_client_for_test(1, 64).await;
    drop(filler);
    let (client_tx, client_rx) = mpsc::channel::<ClientEvent>(128);
    let (mut receiver, _bridge_tx) =
        ConnectReceiver::launch(server, client_rx, reqwest::Client::new(), None).unwrap();
    let send = |message: serde_json::Value| {
        let client_tx = client_tx.clone();
        async move {
            client_tx
                .send(ClientEvent::Message {
                    socket_id: 1,
                    message,
                })
                .await
                .unwrap();
        }
    };
    client_tx.send(ClientEvent::Connected(1)).await.unwrap();
    send(serde_json::json!({ "command": "startSession" })).await;
    let started = heard_on(&mut wire, Duration::from_millis(500)).await;
    let id = started[0]["sessionId"].clone();

    send(serde_json::json!({ "command": "endSession", "sessionId": id, "stopCasting": false }))
        .await;
    send(serde_json::json!({ "command": "next", "requestId": 8 })).await;
    let suspended = heard_on(&mut wire, Duration::from_millis(500)).await;
    send(serde_json::json!({ "command": "resumeSession", "sessionId": id })).await;
    send(serde_json::json!({ "command": "next", "requestId": 9 })).await;
    let resumed = heard_on(&mut wire, Duration::from_millis(500)).await;
    receiver.shutdown(SHUTDOWN_DEADLINE).await;

    // Sorted: the session's answers and the step's are written by two arms of the loop.
    let commands = |heard: &[serde_json::Value]| -> Vec<String> {
        let mut commands: Vec<String> = heard.iter().map(command_of).collect();
        commands.sort();
        commands
    };
    assert_eq!(commands(&suspended), vec!["notifySessionEnded"]);
    assert_eq!(
        commands(&resumed),
        vec!["notifyRequestResult#9", "notifySessionResumed"]
    );
}
