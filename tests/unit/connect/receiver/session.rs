//! Tests for `src/connect/receiver/session.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn session_id_is_none_on_entropy_failure() {
    assert_eq!(generate_session_id_with(|_| false), None);
}

/// What the session asked of the routing loop since the last look, in order.
fn session_effects(outbox: &Outbox) -> Vec<SessionNotifyEvent> {
    outbox
        .take()
        .into_iter()
        .filter_map(|effect| match effect {
            crate::connect::receiver::outbox::Effect::Session(event) => Some(event),
            _ => None,
        })
        .collect()
}

/// A session started on `socket_id`, with the outbox its effects go to.
fn started_hearing(socket_id: u32) -> (ReceiverSession, String, Outbox) {
    let outbox = Outbox::default();
    let mut session = ReceiverSession::new(outbox.clone());
    session.handle_command(socket_id, "startSession", &serde_json::json!({}));
    let mut id = None;
    for event in session_effects(&outbox) {
        if let SessionNotifyEvent::Unicast { message, .. } = event
            && message["command"] == "notifySessionStarted"
        {
            id = message["sessionId"].as_str().map(str::to_string);
        }
    }
    (session, id.expect("a startSession is answered"), outbox)
}

#[test]
fn a_receiver_going_away_tells_the_live_session_it_is_over() {
    let (mut session, id, _) = started_hearing(7);

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

/// The ends the receiver's services were told of, in order.
fn ends_heard(outbox: &Outbox) -> Vec<SessionEnd> {
    let mut ends = Vec::new();
    for event in session_effects(outbox) {
        if let SessionNotifyEvent::Internal(SessionInternalEvent::SessionEnded(end)) = event {
            ends.push(end);
        }
    }
    ends
}

#[test]
fn the_last_phone_gone_leaves_the_session_to_its_resume() {
    let (mut session, id, heard) = started_hearing(7);

    session.handle_disconnect(7);

    assert_eq!(ends_heard(&heard), vec![SessionEnd::Suspended]);
    session.handle_command(8, "resumeSession", &serde_json::json!({ "sessionId": id }));
    assert_eq!(session.state, ReceiverSessionState::Active);
}

#[test]
fn an_end_that_stops_casting_is_over_and_one_that_does_not_is_a_suspension() {
    let (mut stopped, id, heard) = started_hearing(7);
    stopped.handle_command(
        7,
        "endSession",
        &serde_json::json!({ "sessionId": id, "stopCasting": true }),
    );
    assert_eq!(ends_heard(&heard), vec![SessionEnd::Over]);

    let (mut paused, id, heard) = started_hearing(7);
    paused.handle_command(
        7,
        "endSession",
        &serde_json::json!({ "sessionId": id, "stopCasting": false }),
    );
    assert_eq!(ends_heard(&heard), vec![SessionEnd::Suspended]);
}

/// Over, not suspended: TIDAL's receiver drops its queue on the switch.
#[test]
fn a_new_session_ends_the_one_held_for_good() {
    let (mut live, _, heard) = started_hearing(7);
    live.handle_command(8, "startSession", &serde_json::json!({}));
    assert_eq!(ends_heard(&heard), vec![SessionEnd::Over]);

    let (mut suspended, _, heard) = started_hearing(7);
    suspended.handle_disconnect(7);
    suspended.handle_command(8, "startSession", &serde_json::json!({}));
    assert_eq!(
        ends_heard(&heard),
        vec![SessionEnd::Suspended, SessionEnd::Over]
    );
}

/// Each phone of the replaced session hears it is over and is closed, as TIDAL's receiver tells
/// it; the socket that starts the new one keeps its connection, a member of the old one or not.
#[test]
fn a_new_session_tells_the_phones_of_the_one_it_replaces_and_closes_them() {
    for starter in [9, 8] {
        let (mut session, id, heard) = started_hearing(7);
        session.handle_command(8, "resumeSession", &serde_json::json!({ "sessionId": id }));
        session_effects(&heard);

        session.handle_command(starter, "startSession", &serde_json::json!({}));

        let told: Vec<String> = session_effects(&heard)
            .into_iter()
            .filter_map(|event| match event {
                SessionNotifyEvent::Unicast { socket_id, message } => Some(format!(
                    "{socket_id}:{}:{}",
                    message["command"].as_str().unwrap_or("?"),
                    message["suspended"]
                )),
                SessionNotifyEvent::Close { socket_id } => Some(format!("{socket_id}:close")),
                SessionNotifyEvent::Internal(_) => None,
            })
            .collect();
        let displaced = [7, 8].into_iter().filter(|&socket| socket != starter);
        let mut expected: Vec<String> = displaced
            .flat_map(|socket| {
                [
                    format!("{socket}:notifySessionEnded:false"),
                    format!("{socket}:close"),
                ]
            })
            .collect();
        expected.push(format!("{starter}:notifySessionStarted:null"));
        assert_eq!(told, expected, "started on socket {starter}");
    }
}

#[test]
fn a_socket_is_a_member_only_of_the_live_session_it_started_or_resumed() {
    let (mut session, id, _heard) = started_hearing(7);
    assert_eq!(session.admission(7), Admission::Member);
    assert_eq!(
        session.admission(8),
        Admission::Outsider,
        "a socket that neither started nor resumed it"
    );

    session.handle_command(8, "resumeSession", &serde_json::json!({ "sessionId": id }));
    assert_eq!(session.admission(8), Admission::Member);

    session.handle_command(9, "startSession", &serde_json::json!({}));
    assert_eq!(session.admission(9), Admission::Member);
    for replaced in [7, 8] {
        assert_eq!(
            session.admission(replaced),
            Admission::Outsider,
            "socket {replaced} of the session the new one replaced"
        );
    }
}

/// Each refused end is answered with an error, as TIDAL's receiver does.
#[test]
fn an_end_that_does_not_name_the_live_session_ends_nothing() {
    let (mut session, _id, heard) = started_hearing(7);
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
        session.handle_command(socket_id, "endSession", &payload);
    }

    let mut errors = Vec::new();
    let mut ends = 0;
    for event in session_effects(&heard) {
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

/// The `notifySessionEnded` notices and closes the session asked for, in order.
fn end_told(outbox: &Outbox) -> Vec<String> {
    session_effects(outbox)
        .into_iter()
        .filter_map(|event| match event {
            SessionNotifyEvent::Unicast { socket_id, message }
                if message["command"] == "notifySessionEnded" =>
            {
                Some(format!("{socket_id}:ended"))
            }
            SessionNotifyEvent::Close { socket_id } => Some(format!("{socket_id}:close")),
            _ => None,
        })
        .collect()
}

/// Socket 7 started the session, 8 resumed it and 9 ends it. As TIDAL's receiver does, each of the
/// three hears the end and is closed behind it when casting stops; otherwise only the leaver is.
#[test]
fn an_end_tells_and_closes_the_session_and_its_sender_and_a_suspension_closes_the_sender_only() {
    for (stop_casting, expected) in [
        (
            true,
            vec![
                "7:ended", "7:close", "8:ended", "8:close", "9:ended", "9:close",
            ],
        ),
        (false, vec!["7:ended", "8:ended", "9:ended", "9:close"]),
    ] {
        let (mut session, id, heard) = started_hearing(7);
        session.handle_command(8, "resumeSession", &serde_json::json!({ "sessionId": id }));
        session.handle_command(
            9,
            "endSession",
            &serde_json::json!({ "sessionId": id, "stopCasting": stop_casting }),
        );
        assert_eq!(end_told(&heard), expected, "stopCasting: {stop_casting}");
    }
}

#[test]
fn the_members_are_the_sockets_of_the_live_session_only() {
    let (mut session, id, _heard) = started_hearing(7);
    assert_eq!(session.members(), vec![7]);

    session.handle_command(8, "resumeSession", &serde_json::json!({ "sessionId": id }));
    assert_eq!(session.members(), vec![7, 8]);

    session.handle_command(
        7,
        "endSession",
        &serde_json::json!({ "sessionId": id, "stopCasting": false }),
    );
    assert!(session.members().is_empty(), "a suspended session has none");
}

/// The close that follows a suspending end reaches the routing loop as the disconnect of the
/// socket: the suspension is already told, and is not told a second time.
#[test]
fn the_disconnect_of_a_socket_closed_after_an_end_ends_nothing_again() {
    let (mut session, id, heard) = started_hearing(7);
    session.handle_command(
        7,
        "endSession",
        &serde_json::json!({ "sessionId": id, "stopCasting": false }),
    );

    session.handle_disconnect(7);

    assert_eq!(ends_heard(&heard), vec![SessionEnd::Suspended]);
}
