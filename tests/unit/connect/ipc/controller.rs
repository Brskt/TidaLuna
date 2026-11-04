//! Tests for `src/connect/ipc/controller.rs`, attached to it by `#[path]`.

use std::cell::RefCell;

use serde_json::json;

use super::*;
use crate::connect::controller::server_info::ServerInfoClaim;
use crate::connect::mdns::browser::BrowserEvent;
use crate::connect::testing::{MockWsServer, live_controller};
use crate::logging::take_lines;

fn device() -> MdnsDevice {
    crate::connect::testing::device("d")
}

#[derive(Default)]
struct Recorder {
    frames: RefCell<Vec<serde_json::Value>>,
    /// The `suspended` flag of every session end posted.
    ends: RefCell<Vec<bool>>,
}

impl Page for Recorder {
    fn notification(&self, frame: &serde_json::Value) {
        self.frames.borrow_mut().push(frame.clone());
    }

    fn session_event(&self, event: &ControllerSessionEvent, _generation: u64) {
        if let ControllerSessionEvent::SessionEnded { suspended } = event {
            self.ends.borrow_mut().push(*suspended);
        }
    }
}

fn server_error(description: &str) -> WsClientEvent {
    WsClientEvent::Message(json!({
        "command": "notifyQueueServerError",
        "errorCode": 401,
        "details": { "description": description },
    }))
}

fn media(custom_data: &str) -> WsClientEvent {
    WsClientEvent::Message(json!({
        "command": "notifyMediaChanged",
        "mediaInfo": { "itemId": "i", "mediaId": "1", "customData": custom_data },
    }))
}

/// The page and the controller's snapshot read the frame the loop took in, parsed: a loop that
/// stopped parsing it would hand both TIDAL's text again, and its error refresh would throw on it.
#[tokio::test]
async fn a_frame_is_parsed_before_the_controller_and_the_page_read_it() {
    let mut server = MockWsServer::new().await.unwrap();
    let (ctrl, conn_gen, _conn) = live_controller(&mut server, &device()).await;
    let page = Recorder::default();

    assert!(relay_event(
        &ctrl,
        conn_gen,
        server_error(r#"{"status":401,"subStatus":11003}"#),
        &page
    ));
    assert!(relay_event(
        &ctrl,
        conn_gen,
        media(r#"{"audioQuality":"HI_RES_LOSSLESS"}"#),
        &page
    ));

    let frames = page.frames.borrow();
    assert_eq!(frames[0]["details"]["description"]["subStatus"], 11003);
    assert_eq!(
        frames[1]["mediaInfo"]["customData"]["audioQuality"],
        "HI_RES_LOSSLESS"
    );
    let snapshot = ctrl.lock().unwrap().last_media().cloned().expect("media");
    assert_eq!(
        snapshot.custom_data.expect("custom data")["audioQuality"],
        "HI_RES_LOSSLESS"
    );
}

/// A frame of a connection the controller moved past is dropped as it is, before anything reads it:
/// the page would hear a device it no longer casts to, and parsing it would trace a fault of a
/// device nothing is listening to.
#[tokio::test]
async fn a_frame_of_a_superseded_connection_is_dropped_unread() {
    let mut server = MockWsServer::new().await.unwrap();
    let (ctrl, conn_gen, _conn) = live_controller(&mut server, &device()).await;
    let page = Recorder::default();
    let stale = conn_gen.wrapping_sub(1);
    let _ = take_lines();

    assert!(!relay_event(&ctrl, stale, media("{}"), &page));
    assert!(page.frames.borrow().is_empty());
    assert!(!relay_event(
        &ctrl,
        stale,
        server_error("not json {"),
        &page
    ));
    assert_eq!(take_lines(), Vec::<String>::new());
}

/// TIDAL's own controller throws on a `description` that is not JSON and the page never hears of
/// the frame: relayed anyway, the page would show its generic error toast for each. The controller
/// still hears the device's error, or the token it reports failing would not be handed over again.
#[tokio::test]
async fn a_description_that_is_not_json_does_not_reach_the_page() {
    let mut server = MockWsServer::new().await.unwrap();
    let (ctrl, conn_gen, _conn) = live_controller(&mut server, &device()).await;
    let page = Recorder::default();
    let deliver = || {
        ctrl.lock().unwrap().deliver_server_info(
            "a",
            None,
            json!({ "command": "updateServerInfo" }),
        )
    };
    assert!(matches!(deliver(), ServerInfoClaim::Queued(_)));
    assert!(matches!(deliver(), ServerInfoClaim::Held));

    for text in ["revoked", ""] {
        assert!(relay_event(&ctrl, conn_gen, server_error(text), &page));
        assert!(page.frames.borrow().is_empty(), "{text:?}");
        assert!(
            matches!(deliver(), ServerInfoClaim::Queued(_)),
            "{text:?}: the device reported its server info failing"
        );
    }
}

/// A `customData` that is not JSON costs the track display if the frame is dropped: it is relayed
/// without the field. Each of the two text fields is its own outage, one `verr!` line apiece, and
/// a frame that parses ends only its own field's.
#[tokio::test]
async fn each_text_field_is_reported_once_per_outage() {
    let mut server = MockWsServer::new().await.unwrap();
    let (ctrl, conn_gen, _conn) = live_controller(&mut server, &device()).await;
    let page = Recorder::default();
    let _ = take_lines();
    let relay = |event| assert!(relay_event(&ctrl, conn_gen, event, &page));

    for _ in 0..3 {
        relay(server_error("{"));
    }
    assert_eq!(take_lines().len(), 1);
    for _ in 0..3 {
        relay(media("{"));
    }
    assert_eq!(take_lines().len(), 1, "the other field has its own outage");
    let frames = page.frames.borrow().len();
    assert_eq!(
        frames, 3,
        "the track frames still reach the page, the error frames do not"
    );

    relay(server_error("{}"));
    relay(media("{"));
    assert_eq!(
        take_lines().len(),
        0,
        "a description that parses does not end the customData outage"
    );
    relay(media("{}"));
    relay(media("{"));
    assert_eq!(take_lines().len(), 1, "its own recovery does");
    assert!(
        page.frames.borrow()[0]["mediaInfo"]
            .get("customData")
            .is_none(),
        "relayed without the field"
    );
}

fn discovered(port: u16) -> BrowserEvent {
    BrowserEvent::DeviceFound(MdnsDevice { port, ..device() })
}

/// A failed connect reaches the page as the end of a session whose old socket is gone, whichever
/// way it failed: a bare `connect_failed` would leave TIDAL on the remote player. A live session
/// is left alone, and the end is told once.
#[tokio::test]
async fn a_failed_connect_ends_a_dead_session_in_the_page() {
    let mut server = MockWsServer::new().await.unwrap();
    let resume = ConnectFailure::Resume("refused".into());
    let unreachable = ConnectFailure::Unresolved("d".into());

    let (ctrl, _, _conn) = live_controller(&mut server, &device()).await;
    let page = Recorder::default();
    assert!(
        !settle_failure(Some(&ctrl), &resume, &page),
        "a live session hides the failure"
    );
    assert!(page.ends.borrow().is_empty());

    ctrl.lock().unwrap().ws_client().unwrap().shutdown();
    assert!(settle_failure(Some(&ctrl), &unreachable, &page));
    assert_eq!(*page.ends.borrow(), [false], "the end TIDAL is owed");

    assert!(settle_failure(Some(&ctrl), &resume, &page));
    assert_eq!(*page.ends.borrow(), [false], "and only once");
}

/// A connect still dialling when the user disconnects must not install behind the disconnect, which
/// would start the session the user just ended: the disconnect ends that attempt first.
#[tokio::test]
async fn a_disconnect_ends_the_connect_still_dialling() {
    let mut server = MockWsServer::new().await.unwrap();
    let (ctrl, _, _conn) = live_controller(&mut server, &device()).await;
    let flights = ConnectFlights::new();
    let page = Recorder::default();

    let dialling = flights.run(
        "d",
        std::future::pending::<Result<(), ConnectFailure>>(),
        |_| Verdict::done(true),
        |()| {},
        |_| {},
    );
    let disconnecting = async { end_session(&flights, &ctrl, true, &page) };
    let both = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(dialling, disconnecting)
    })
    .await;

    let (settled, ()) = both.expect("the attempt is cancelled, not left dialling");
    assert!(matches!(settled.result, Err(ConnectFailure::Cancelled)));
    assert_eq!(
        *page.ends.borrow(),
        [false],
        "and the session is ended in the page"
    );
}

/// What the resume wait read is the session's to hear, and first: the device sent it before the
/// frames that follow on the channel, and a loop that skipped it would lose a state change unseen.
#[tokio::test]
async fn the_frames_the_resume_wait_read_are_relayed_before_the_ones_after_it() {
    let mut server = MockWsServer::new().await.unwrap();
    let (ctrl, conn_gen, _conn) = live_controller(&mut server, &device()).await;
    let page = Recorder::default();
    let (tx, rx) = mpsc::channel(4);
    tx.send(media(r#"{"n":2}"#)).await.unwrap();
    tx.send(WsClientEvent::ConnectionLost).await.unwrap();

    read_connection(&ctrl, conn_gen, vec![media(r#"{"n":1}"#)], rx, &page).await;

    let heard: Vec<_> = page
        .frames
        .borrow()
        .iter()
        .map(|frame| frame["mediaInfo"]["customData"]["n"].as_i64())
        .collect();
    assert_eq!(heard, [Some(1), Some(2)]);
}

const IMPATIENT_LINK: Link = Link {
    disable_tls: true,
    resume_deadline: std::time::Duration::from_millis(200),
};

/// A session TIDAL was told started and then lost, or suspended by the device, with the device
/// found again by the browse: what a reconnect after either finds.
async fn gone_session(
    server: &mut MockWsServer,
    suspended: bool,
) -> (
    Arc<Mutex<TidalConnectController>>,
    crate::connect::testing::MockWsConnection,
) {
    let (ctrl, _, conn) = live_controller(server, &device()).await;
    let lapse = if suspended {
        WsClientEvent::Message(json!({
            "command": "notifySessionEnded", "sessionId": "s1", "suspended": true,
        }))
    } else {
        WsClientEvent::ConnectionLost
    };
    {
        let mut guard = ctrl.lock().unwrap();
        assert!(guard.handle_ws_event(&lapse).is_some());
        assert!(!guard.is_connected(), "TIDAL was told the session is gone");
        guard.handle_browser_event(discovered(server.port()));
    }
    (ctrl, conn)
}

/// A resume of a lost or suspended session is confirmed by the device or fails the connect (silent,
/// refusing, dropping, resuming another session), releasing TIDAL's queue restore; the end owed
/// after a loss is announced with it.
#[tokio::test]
async fn a_resume_of_a_gone_session_the_device_does_not_confirm_fails_the_connect() {
    let behaviours = [
        "stays silent",
        "refuses",
        "drops the socket",
        "resumes another session",
    ];
    for suspended in [false, true] {
        for behaviour in behaviours {
            let what = format!("{behaviour} (suspended: {suspended})");
            let mut server = MockWsServer::new().await.unwrap();
            let (ctrl, _old_conn) = gone_session(&mut server, suspended).await;

            let dialling = tokio::spawn({
                let ctrl = ctrl.clone();
                async move {
                    dial_device(Some(ctrl), device(), &IMPATIENT_LINK)
                        .await
                        .err()
                }
            });
            let mut conn = server.accept().await.expect("the new socket is dialled");
            let asked = tokio::time::timeout(std::time::Duration::from_secs(2), conn.recv())
                .await
                .unwrap_or_else(|_| panic!("{what}: asked to resume, not left waiting"));
            assert_eq!(
                asked.expect("a frame")["command"],
                "resumeSession",
                "{what}"
            );
            let _held = match behaviour {
                "refuses" => {
                    conn.send(json!({
                        "command": "notifySessionError",
                        "requestedCommand": "resumeSession",
                        "error": "No Session",
                    }))
                    .await;
                    Some(conn)
                }
                "resumes another session" => {
                    conn.send(json!({
                        "command": "notifySessionResumed", "sessionId": "other",
                    }))
                    .await;
                    Some(conn)
                }
                "drops the socket" => {
                    drop(conn);
                    None
                }
                _ => Some(conn),
            };

            let failure = tokio::time::timeout(std::time::Duration::from_secs(2), dialling)
                .await
                .unwrap_or_else(|_| panic!("{what}: the wait is bounded"))
                .unwrap()
                .unwrap_or_else(|| panic!("{what}: the connect has to fail"));
            assert!(matches!(failure, ConnectFailure::Resume(_)), "{what}");
            if behaviour == "refuses" {
                let reason = failure.message();
                assert!(reason.contains("No Session"), "{what}: {reason}");
            }

            let page = Recorder::default();
            assert!(
                settle_failure(Some(&ctrl), &failure, &page),
                "{what}: TIDAL is told the connect failed"
            );
            let owed: &[bool] = if suspended { &[] } else { &[false] };
            assert_eq!(*page.ends.borrow(), owed, "{what}");
            assert!(
                ctrl.lock().unwrap().ws_client().is_none(),
                "{what}: nothing is left to resume"
            );
        }
    }
}

/// A connect that arrives while the device has not yet confirmed a resume is the same attempt: its
/// own dial would put a second socket in the device's hands while the first still carries the
/// unconfirmed resume, a close the device reads as the session ending.
#[tokio::test]
async fn a_second_connect_during_a_resume_wait_follows_the_first() {
    let mut server = MockWsServer::new().await.unwrap();
    let (ctrl, _old_conn) = gone_session(&mut server, false).await;
    let flights = ConnectFlights::new();
    let page = Recorder::default();
    let attempt = || {
        flights.run(
            "d",
            dial_device(Some(ctrl.clone()), device(), &IMPATIENT_LINK),
            |dialled| match dialled {
                Ok(dialled) => {
                    discard_dialled(dialled);
                    Verdict::done(true)
                }
                Err(failure) => Verdict {
                    announce: settle_failure(Some(&ctrl), &failure, &page),
                    result: Err(failure),
                },
            },
            discard_dialled,
            |_| {},
        )
    };

    let (first, (second, redialled)) = tokio::join!(attempt(), async {
        let mut conn = server.accept().await.expect("the first dials");
        let asked = tokio::time::timeout(std::time::Duration::from_secs(2), conn.recv())
            .await
            .expect("asked to resume, not left waiting");
        assert_eq!(asked.expect("a frame")["command"], "resumeSession");
        let second = attempt().await;
        let redialled =
            tokio::time::timeout(std::time::Duration::from_millis(150), server.accept())
                .await
                .is_ok();
        (second, redialled)
    });

    assert!(!redialled, "the second request dialled again");
    for settled in [first, second] {
        assert!(matches!(settled.result, Err(ConnectFailure::Resume(_))));
    }
}

fn other_device(port: u16) -> MdnsDevice {
    MdnsDevice {
        fullname: "e".into(),
        port,
        ..device()
    }
}

/// A start is answered by the device and by nothing else: no deadline (a device may wait on its
/// resources first), and the connect stays one attempt until then.
#[tokio::test]
async fn a_start_waits_for_the_device_without_a_deadline() {
    let mut server = MockWsServer::new().await.unwrap();
    let ctrl = Arc::new(Mutex::new(TidalConnectController::new(None)));
    ctrl.lock()
        .unwrap()
        .handle_browser_event(discovered(server.port()));
    let mut dialling = tokio::spawn({
        let ctrl = ctrl.clone();
        async move {
            dial_device(Some(ctrl), device(), &IMPATIENT_LINK)
                .await
                .is_ok()
        }
    });
    let mut conn = server.accept().await.expect("the new socket is dialled");
    let asked = tokio::time::timeout(std::time::Duration::from_secs(2), conn.recv())
        .await
        .expect("asked to start, not left waiting");
    assert_eq!(asked.expect("a frame")["command"], "startSession");

    let patience = std::time::Duration::from_millis(400);
    assert!(
        tokio::time::timeout(patience, &mut dialling).await.is_err(),
        "the connect answered before the device did"
    );
    conn.send(json!({ "command": "notifySessionStarted", "sessionId": "s9", "joined": true }))
        .await;
    let answered = tokio::time::timeout(std::time::Duration::from_secs(2), dialling).await;
    assert!(answered.expect("answered").unwrap());
}

/// A start the device refuses or drops fails the connect, as a resume does, and ends what TIDAL
/// was told started and lost: C3 alone would leave it on the remote player.
#[tokio::test]
async fn a_start_the_device_refuses_or_drops_fails_the_connect() {
    for suspended in [false, true] {
        for behaviour in ["refuses", "drops the socket"] {
            let what = format!("{behaviour} (suspended: {suspended})");
            let mut server = MockWsServer::new().await.unwrap();
            let (ctrl, _old_conn) = gone_session(&mut server, suspended).await;
            ctrl.lock()
                .unwrap()
                .handle_browser_event(BrowserEvent::DeviceFound(other_device(server.port())));
            let dialling = tokio::spawn({
                let ctrl = ctrl.clone();
                async move {
                    dial_device(Some(ctrl), other_device(1), &IMPATIENT_LINK)
                        .await
                        .err()
                }
            });
            let mut conn = server.accept().await.expect("the new socket is dialled");
            let asked = tokio::time::timeout(std::time::Duration::from_secs(2), conn.recv())
                .await
                .unwrap_or_else(|_| panic!("{what}: asked to start, not left waiting"));
            assert_eq!(asked.expect("a frame")["command"], "startSession", "{what}");
            let _held = if behaviour == "refuses" {
                conn.send(json!({
                    "command": "notifySessionError", "requestedCommand": "startSession",
                    "error": "Illegal state",
                }))
                .await;
                Some(conn)
            } else {
                drop(conn);
                None
            };

            let failure = tokio::time::timeout(std::time::Duration::from_secs(2), dialling)
                .await
                .unwrap_or_else(|_| panic!("{what}: the wait ends with the device's answer"))
                .unwrap()
                .unwrap_or_else(|| panic!("{what}: the connect has to fail"));
            assert!(matches!(failure, ConnectFailure::Start(_)), "{what}");
            if behaviour == "refuses" {
                let reason = failure.message();
                assert!(reason.contains("Illegal state"), "{what}: {reason}");
            }
            let page = Recorder::default();
            assert!(settle_failure(Some(&ctrl), &failure, &page), "{what}");
            let owed: &[bool] = if suspended { &[] } else { &[false] };
            assert_eq!(*page.ends.borrow(), owed, "{what}");
        }
    }
}

/// A connect arriving while a start awaits its device is the same attempt: its own dial would
/// close the socket carrying the start, and the session the device then reports would be unowned.
#[tokio::test]
async fn a_second_connect_during_a_start_follows_the_first() {
    let mut server = MockWsServer::new().await.unwrap();
    let ctrl = Arc::new(Mutex::new(TidalConnectController::new(None)));
    ctrl.lock()
        .unwrap()
        .handle_browser_event(discovered(server.port()));
    let flights = ConnectFlights::new();
    let attempt = || {
        flights.run(
            "d",
            dial_device(Some(ctrl.clone()), device(), &IMPATIENT_LINK),
            |dialled| {
                dialled.map(discard_dialled).ok();
                Verdict::done(true)
            },
            discard_dialled,
            |_| {},
        )
    };

    let (first, (second, redialled)) = tokio::join!(attempt(), async {
        let mut conn = server.accept().await.expect("the first dials");
        let asked = tokio::time::timeout(std::time::Duration::from_secs(2), conn.recv())
            .await
            .expect("asked to start, not left waiting");
        assert_eq!(asked.expect("a frame")["command"], "startSession");
        let second = attempt();
        tokio::pin!(second);
        let waiting = tokio::time::timeout(std::time::Duration::from_millis(100), &mut second);
        assert!(waiting.await.is_err(), "answered before the device did");
        conn.send(json!({ "command": "notifySessionStarted", "sessionId": "s9", "joined": false }))
            .await;
        let second = second.await;
        let redialled =
            tokio::time::timeout(std::time::Duration::from_millis(150), server.accept())
                .await
                .is_ok();
        (second, redialled)
    });

    assert!(!redialled, "the second request dialled again");
    for settled in [first, second] {
        assert!(settled.result.is_ok());
    }
}

/// A controller with no session and `device()` found, the state of a first connect.
fn fresh_controller(server: &MockWsServer) -> Arc<Mutex<TidalConnectController>> {
    let ctrl = Arc::new(Mutex::new(TidalConnectController::new(None)));
    ctrl.lock()
        .unwrap()
        .handle_browser_event(discovered(server.port()));
    ctrl
}

/// The credential a start carries is read when the command is sent, not when the request began: a
/// `set_auth` during the resolve and dial wait would otherwise be dropped, and the device would
/// join the session of the old account.
#[tokio::test]
async fn a_credential_set_during_the_wait_reaches_the_start() {
    let mut server = MockWsServer::new().await.unwrap();
    let ctrl = Arc::new(Mutex::new(TidalConnectController::new(None)));
    ctrl.lock().unwrap().set_session_credential(Some("old"));
    let dialling = tokio::spawn({
        let ctrl = ctrl.clone();
        async move {
            dial_device(Some(ctrl), device(), &IMPATIENT_LINK)
                .await
                .err()
        }
    });
    tokio::task::yield_now().await;
    ctrl.lock().unwrap().set_session_credential(Some("new"));
    ctrl.lock()
        .unwrap()
        .handle_browser_event(discovered(server.port()));

    let mut conn = server.accept().await.expect("the new socket is dialled");
    let asked = tokio::time::timeout(std::time::Duration::from_secs(2), conn.recv())
        .await
        .expect("asked to start, not left waiting");
    assert_eq!(asked.expect("a frame")["sessionCredential"], "new");
    dialling.abort();
}

/// What the device sent ahead of its start notice is the session's to hear after the install; the
/// notice is the install's to announce and is not replayed.
#[tokio::test]
async fn the_frames_before_a_start_notice_are_kept_and_the_notice_is_not() {
    let mut server = MockWsServer::new().await.unwrap();
    let ctrl = fresh_controller(&server);
    let dialling = tokio::spawn(async move {
        let dialled = dial_device(Some(ctrl), device(), &IMPATIENT_LINK)
            .await
            .ok()?;
        dialled.ws.shutdown();
        let Established::Started(started) = dialled.established else {
            return None;
        };
        let frames = started.frames.into_iter().map(|event| match event {
            WsClientEvent::Message(frame) => frame["command"].clone(),
            _ => json!(null),
        });
        Some((
            started.session_id,
            started.joined,
            frames.collect::<Vec<_>>(),
        ))
    });
    let mut conn = server.accept().await.expect("the new socket is dialled");
    let asked = tokio::time::timeout(std::time::Duration::from_secs(2), conn.recv())
        .await
        .expect("asked to start, not left waiting");
    assert_eq!(asked.expect("a frame")["command"], "startSession");
    conn.send(json!({ "command": "notifyPlayerStatusChanged" }))
        .await;
    conn.send(json!({ "command": "notifySessionStarted", "sessionId": "s9", "joined": true }))
        .await;

    let started = tokio::time::timeout(std::time::Duration::from_secs(2), dialling)
        .await
        .expect("answered")
        .unwrap()
        .expect("a started session");
    assert_eq!(
        started,
        (
            "s9".to_string(),
            true,
            vec![json!("notifyPlayerStatusChanged")]
        )
    );
}

/// A device sending more than the wait sets aside before it answers is not answering: the connect
/// fails with the cap's own error, for a start, which has no deadline, as for a resume.
#[tokio::test]
async fn a_device_flooding_a_wait_fails_the_connect() {
    for resume in [false, true] {
        let mut server = MockWsServer::new().await.unwrap();
        let mut old_conn = None;
        let ctrl = if resume {
            let (ctrl, conn) = gone_session(&mut server, false).await;
            old_conn = Some(conn);
            ctrl
        } else {
            fresh_controller(&server)
        };
        let dialling = tokio::spawn(async move {
            dial_device(Some(ctrl), device(), &IMPATIENT_LINK)
                .await
                .err()
        });
        let mut conn = server.accept().await.expect("the new socket is dialled");
        let asked = tokio::time::timeout(std::time::Duration::from_secs(2), conn.recv())
            .await
            .expect("asked, not left waiting");
        assert!(asked.is_some(), "resume: {resume}");
        for _ in 0..65 {
            conn.send(json!({ "command": "notifyPlayerStatusChanged" }))
                .await;
        }
        let failure = tokio::time::timeout(std::time::Duration::from_secs(2), dialling)
            .await
            .expect("the wait is bounded")
            .unwrap()
            .expect("the connect has to fail");
        assert!(failure.message().contains("too much"), "resume: {resume}");
        drop(old_conn);
    }
}

/// A failure that took the attempt's purpose away (`Cancelled`, `Superseded`) reaching the settle
/// does not end the session it found: whoever took the purpose away owns the outcome.
#[tokio::test]
async fn a_resume_that_found_its_session_moved_ends_nothing() {
    let mut server = MockWsServer::new().await.unwrap();
    let (ctrl, _old_conn) = gone_session(&mut server, false).await;
    let plan = ctrl.lock().unwrap().resume_plan("d").expect("a plan");
    let port = server.port();
    let socket = |tx| async move {
        Arc::new(
            WsClient::connect("127.0.0.1", port, true, tx)
                .await
                .unwrap(),
        )
    };
    let (tx, _rx) = mpsc::channel(8);
    let other = socket(tx);
    let (other, _other_conn) = tokio::join!(other, server.accept());
    let (tx, _rx) = mpsc::channel(8);
    let (late, _late_conn) = tokio::join!(socket(tx), server.accept());
    {
        let mut guard = ctrl.lock().unwrap();
        let _ = guard.adopt_started(other, &other_device(1), "s2".into(), false);
        assert!(
            guard
                .handle_ws_event(&WsClientEvent::ConnectionLost)
                .is_some()
        );
    }
    let moved = ctrl.lock().unwrap().adopt_resumed(late, &device(), &plan);
    let Err(failure) = moved else {
        panic!("the session moved meanwhile");
    };

    let page = Recorder::default();
    assert!(!settle_failure(Some(&ctrl), &failure, &page));
    assert!(page.ends.borrow().is_empty());
    assert!(ctrl.lock().unwrap().ws_client().is_some());
}

/// A resume the device never confirms ends the attempt, and the socket dialled for it with it:
/// nothing else holds that socket; left open, it would be a connection nobody reads or closes.
#[tokio::test]
async fn a_resume_the_device_never_confirms_closes_the_socket_it_dialled() {
    let mut server = MockWsServer::new().await.unwrap();
    let (ctrl, _, _old_conn) = live_controller(&mut server, &device()).await;
    ctrl.lock()
        .unwrap()
        .handle_browser_event(discovered(server.port()));

    let dialling = tokio::spawn({
        let ctrl = ctrl.clone();
        async move {
            dial_device(Some(ctrl), device(), &IMPATIENT_LINK)
                .await
                .err()
        }
    });
    let mut new_conn = server.accept().await.expect("the new socket is dialled");
    let asked = tokio::time::timeout(std::time::Duration::from_secs(2), new_conn.recv())
        .await
        .expect("asked to resume, not left waiting");
    assert_eq!(asked.expect("asked to resume")["command"], "resumeSession");

    let failure = dialling.await.unwrap().expect("nobody confirmed");
    assert!(matches!(failure, ConnectFailure::Resume(_)));
    let closed = tokio::time::timeout(std::time::Duration::from_secs(2), new_conn.recv()).await;
    assert!(
        matches!(closed, Ok(None)),
        "the dialled socket is closed, not left open"
    );
}
