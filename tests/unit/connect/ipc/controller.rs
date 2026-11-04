//! Tests for `src/connect/ipc/controller.rs`, attached to it by `#[path]`.

use std::cell::RefCell;

use serde_json::json;

use super::*;
use crate::connect::controller::server_info::ServerInfoClaim;
use crate::connect::mdns::browser::BrowserEvent;
use crate::connect::testing::{MockWsServer, live_controller};
use crate::connect::types::DeviceType;
use crate::logging::take_lines;

fn device() -> MdnsDevice {
    MdnsDevice {
        addresses: vec!["127.0.0.1".into()],
        friendly_name: "n".into(),
        fullname: "d".into(),
        id: "id".into(),
        port: 1,
        device_type: DeviceType::TidalConnect,
    }
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

/// A resume the device did not confirm reaches the page as the end of a session whose old socket
/// is gone: a bare `connect_failed` would leave TIDAL playing on the remote player. A live session
/// is left alone, and so is one after a dial that never reached the device.
#[tokio::test]
async fn a_failed_resume_ends_a_dead_session_in_the_page() {
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
    assert!(
        page.ends.borrow().is_empty(),
        "no resume was asked of the device"
    );

    assert!(settle_failure(Some(&ctrl), &resume, &page));
    assert_eq!(*page.ends.borrow(), [false], "the end TIDAL is owed");
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
    assert!(failure.is_resume());
    let closed = tokio::time::timeout(std::time::Duration::from_secs(2), new_conn.recv()).await;
    assert!(
        matches!(closed, Ok(None)),
        "the dialled socket is closed, not left open"
    );
}
