//! Tests for `src/connect/receiver/client_comm.rs`, attached to it by `#[path]`.

use super::*;

/// A phone's messages, session and resource ones aside, plus an unknown one.
const PHONE_COMMANDS: [&str; 18] = [
    "play",
    "pause",
    "stop",
    "seek",
    "loadCloudQueue",
    "loadMedia",
    "loadMediaInfo",
    "refreshQueue",
    "updateServerInfo",
    "next",
    "previous",
    "selectQueueItem",
    "setRepeatMode",
    "setShuffle",
    "setVolume",
    "setMute",
    "notifySessionError",
    "a command no receiver knows",
];

const SESSION_COMMANDS: [&str; 3] = ["startSession", "resumeSession", "endSession"];

fn classified(command: &str, admission: Admission) -> DispatchedCommand {
    classify_command(
        7,
        command,
        serde_json::json!({ "command": command }),
        admission,
    )
}

#[test]
fn an_outsider_may_only_start_resume_or_end_a_session() {
    for command in PHONE_COMMANDS {
        assert!(
            matches!(
                classified(command, Admission::Outsider),
                DispatchedCommand::Dropped { .. }
            ),
            "{command} from an outsider reached a service"
        );
    }
    for command in SESSION_COMMANDS {
        assert!(
            matches!(
                classified(command, Admission::Outsider),
                DispatchedCommand::Session { socket_id: 7, .. }
            ),
            "{command} from an outsider never reached the session"
        );
    }
}

#[test]
fn a_members_commands_reach_their_service() {
    for command in ["play", "pause", "stop", "seek"] {
        assert!(
            matches!(
                classified(command, Admission::Member),
                DispatchedCommand::Playback { .. }
            ),
            "{command}"
        );
    }
    for command in &PHONE_COMMANDS[4..16] {
        assert!(
            matches!(
                classified(command, Admission::Member),
                DispatchedCommand::Queue { socket_id: 7, .. }
            ),
            "{command}"
        );
    }
    for command in SESSION_COMMANDS {
        assert!(
            matches!(
                classified(command, Admission::Member),
                DispatchedCommand::Session { .. }
            ),
            "{command}"
        );
    }
}

#[test]
fn a_resource_message_is_dropped_whoever_sends_it() {
    for command in ["requestResources", "releaseResources"] {
        for admission in [Admission::Member, Admission::Outsider] {
            assert!(
                matches!(
                    classified(command, admission),
                    DispatchedCommand::Dropped { .. }
                ),
                "{command} from a {admission:?}"
            );
        }
    }
}
