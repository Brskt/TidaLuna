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

#[tokio::test]
async fn a_socket_is_a_member_only_of_the_live_session_it_started_or_resumed() {
    let (mut session, id, _heard) = started_hearing(7).await;
    assert_eq!(session.admission(7), Admission::Member);
    assert_eq!(
        session.admission(8),
        Admission::Outsider,
        "a socket that neither started nor resumed it"
    );

    session
        .handle_command(8, "resumeSession", &serde_json::json!({ "sessionId": id }))
        .await;
    assert_eq!(session.admission(8), Admission::Member);

    session
        .handle_command(9, "startSession", &serde_json::json!({}))
        .await;
    assert_eq!(session.admission(9), Admission::Member);
    for replaced in [7, 8] {
        assert_eq!(
            session.admission(replaced),
            Admission::Outsider,
            "socket {replaced} of the session the new one replaced"
        );
    }
}

#[tokio::test]
async fn a_suspended_session_admits_nobody_until_it_is_resumed() {
    let (mut session, id, _heard) = started_hearing(7).await;
    session
        .handle_command(
            7,
            "endSession",
            &serde_json::json!({ "sessionId": id, "stopCasting": false }),
        )
        .await;
    assert_eq!(session.admission(7), Admission::Outsider);

    session
        .handle_command(7, "resumeSession", &serde_json::json!({ "sessionId": id }))
        .await;
    assert_eq!(session.admission(7), Admission::Member);
}

#[test]
fn no_session_admits_nobody() {
    let (tx, _rx) = mpsc::channel(1);
    assert_eq!(ReceiverSession::new(tx).admission(7), Admission::Outsider);
}

/// Each refused end is answered with an error, as TIDAL's receiver does.
#[tokio::test]
async fn an_end_that_does_not_name_the_live_session_ends_nothing() {
    let (mut session, _id, mut heard) = started_hearing(7).await;
    for (socket_id, payload) in [
        (8, serde_json::json!({ "stopCasting": true })),
        (
            8,
            serde_json::json!({ "sessionId": "another", "stopCasting": true }),
        ),
        (
            7,
            serde_json::json!({ "sessionId": "another", "stopCasting": false }),
        ),
    ] {
        session
            .handle_command(socket_id, "endSession", &payload)
            .await;
    }

    let mut errors = Vec::new();
    let mut ends = 0;
    while let Ok(event) = heard.try_recv() {
        match event {
            SessionNotifyEvent::Unicast { socket_id, message }
                if message["command"] == "notifySessionError" =>
            {
                errors.push((socket_id, message["requestedCommand"].clone()));
            }
            SessionNotifyEvent::Internal(SessionInternalEvent::SessionEnded(_)) => ends += 1,
            SessionNotifyEvent::Unicast { message, .. }
                if message["command"] == "notifySessionEnded" =>
            {
                ends += 1;
            }
            _ => {}
        }
    }
    assert_eq!(ends, 0, "an end that names another session ended this one");
    assert_eq!(
        errors,
        vec![
            (8, serde_json::json!("endSession")),
            (8, serde_json::json!("endSession")),
            (7, serde_json::json!("endSession")),
        ]
    );
    assert_eq!(session.admission(7), Admission::Member);
}

#[tokio::test]
async fn an_end_that_names_the_live_session_ends_it_whoever_sends_it() {
    let (mut session, id, mut heard) = started_hearing(7).await;
    session
        .handle_command(
            8,
            "endSession",
            &serde_json::json!({ "sessionId": id, "stopCasting": true }),
        )
        .await;
    assert_eq!(ends_heard(&mut heard), vec![SessionEnd::Over]);
    assert_eq!(session.admission(7), Admission::Outsider);
}

/// The sockets a `notifySessionEnded` was written to, in order.
fn end_notices(rx: &mut mpsc::Receiver<SessionNotifyEvent>) -> Vec<u32> {
    let mut sockets = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let SessionNotifyEvent::Unicast { socket_id, message } = event
            && message["command"] == "notifySessionEnded"
        {
            sockets.push(socket_id);
        }
    }
    sockets
}

#[tokio::test]
async fn an_end_is_told_to_the_sockets_of_the_session_and_to_the_one_that_ended_it() {
    for stop_casting in [false, true] {
        let (mut session, id, mut heard) = started_hearing(7).await;
        session
            .handle_command(8, "resumeSession", &serde_json::json!({ "sessionId": id }))
            .await;
        session
            .handle_command(
                9,
                "endSession",
                &serde_json::json!({ "sessionId": id, "stopCasting": stop_casting }),
            )
            .await;
        assert_eq!(
            end_notices(&mut heard),
            vec![7, 8, 9],
            "stopCasting: {stop_casting}"
        );
    }
}

#[tokio::test]
async fn the_members_are_the_sockets_of_the_live_session_only() {
    let (mut session, id, _heard) = started_hearing(7).await;
    assert_eq!(session.members(), vec![7]);

    session
        .handle_command(8, "resumeSession", &serde_json::json!({ "sessionId": id }))
        .await;
    assert_eq!(session.members(), vec![7, 8]);

    session
        .handle_command(
            7,
            "endSession",
            &serde_json::json!({ "sessionId": id, "stopCasting": false }),
        )
        .await;
    assert!(session.members().is_empty(), "a suspended session has none");
}
