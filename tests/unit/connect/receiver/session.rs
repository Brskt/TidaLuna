//! Tests for `src/connect/receiver/session.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn session_id_is_none_on_entropy_failure() {
    assert_eq!(generate_session_id_with(|_| false), None);
}

#[test]
fn session_id_is_uuid_v4_shaped() {
    let id = generate_session_id_with(|buf| {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = i as u8;
        }
        true
    })
    .expect("RNG ok");
    let parts: Vec<&str> = id.split('-').collect();
    assert_eq!(id.len(), 36);
    assert_eq!(parts.len(), 5);
    assert!(parts[2].starts_with('4'), "version nibble must be 4");
    assert!(
        matches!(parts[3].chars().next(), Some('8'..='b')),
        "variant nibble must be 8..b"
    );
}

/// A session started on `socket_id`, with the channel its services hear on.
async fn started_hearing(
    socket_id: u32,
) -> (ReceiverSession, String, mpsc::Receiver<SessionNotifyEvent>) {
    let (tx, mut rx) = mpsc::channel(16);
    let mut session = ReceiverSession::new(tx);
    session
        .handle_command(socket_id, "startSession", &serde_json::json!({}))
        .await;
    let mut id = None;
    while let Ok(event) = rx.try_recv() {
        if let SessionNotifyEvent::Unicast { message, .. } = event
            && message["command"] == "notifySessionStarted"
        {
            id = message["sessionId"].as_str().map(str::to_string);
        }
    }
    (session, id.expect("a startSession is answered"), rx)
}

#[tokio::test]
async fn a_receiver_going_away_tells_the_live_session_it_is_over() {
    let (mut session, id, _) = started_hearing(7).await;

    let revoked = session
        .revoke()
        .expect("a live session has a phone to tell");

    assert_eq!(
        revoked.sockets,
        vec![7],
        "told to the socket holding the session"
    );
    assert_eq!(
        revoked.notice,
        serde_json::json!({
            "command": "notifySessionEnded",
            "sessionId": id,
            "suspended": false,
        })
    );
    assert!(
        session.revoke().is_none(),
        "a session ends once: a second shutdown step has nobody left to tell"
    );
}

#[tokio::test]
async fn a_suspended_session_has_nobody_to_tell() {
    let (mut session, _, _) = started_hearing(7).await;
    session.handle_disconnect(7).await;

    assert!(session.revoke().is_none());
}

#[tokio::test]
async fn no_session_means_no_notice() {
    let (tx, _rx) = mpsc::channel(1);
    assert!(ReceiverSession::new(tx).revoke().is_none());
}
