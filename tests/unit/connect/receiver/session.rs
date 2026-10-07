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

/// The ends the receiver's services were told of, in order.
fn ends_heard(rx: &mut mpsc::Receiver<SessionNotifyEvent>) -> Vec<SessionEnd> {
    let mut ends = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let SessionNotifyEvent::Internal(SessionInternalEvent::SessionEnded(end)) = event {
            ends.push(end);
        }
    }
    ends
}

#[tokio::test]
async fn the_last_phone_gone_leaves_the_session_to_its_resume() {
    let (mut session, id, mut heard) = started_hearing(7).await;

    session.handle_disconnect(7).await;

    assert_eq!(ends_heard(&mut heard), vec![SessionEnd::Suspended]);
    session
        .handle_command(8, "resumeSession", &serde_json::json!({ "sessionId": id }))
        .await;
    assert_eq!(session.state, ReceiverSessionState::Active);
}

#[tokio::test]
async fn an_end_that_stops_casting_is_over_and_one_that_does_not_is_a_suspension() {
    let (mut stopped, id, mut heard) = started_hearing(7).await;
    stopped
        .handle_command(
            7,
            "endSession",
            &serde_json::json!({ "sessionId": id, "stopCasting": true }),
        )
        .await;
    assert_eq!(ends_heard(&mut heard), vec![SessionEnd::Over]);

    let (mut paused, id, mut heard) = started_hearing(7).await;
    paused
        .handle_command(
            7,
            "endSession",
            &serde_json::json!({ "sessionId": id, "stopCasting": false }),
        )
        .await;
    assert_eq!(ends_heard(&mut heard), vec![SessionEnd::Suspended]);
}

/// Over, not suspended: TIDAL's receiver drops its queue on the switch.
#[tokio::test]
async fn a_new_session_ends_the_one_held_for_good() {
    let (mut live, _, mut heard) = started_hearing(7).await;
    live.handle_command(8, "startSession", &serde_json::json!({}))
        .await;
    assert_eq!(ends_heard(&mut heard), vec![SessionEnd::Over]);

    let (mut suspended, _, mut heard) = started_hearing(7).await;
    suspended.handle_disconnect(7).await;
    suspended
        .handle_command(8, "startSession", &serde_json::json!({}))
        .await;
    assert_eq!(
        ends_heard(&mut heard),
        vec![SessionEnd::Suspended, SessionEnd::Over]
    );
}
