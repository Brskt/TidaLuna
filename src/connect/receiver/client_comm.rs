use crate::connect::receiver::session::Admission;
use crate::connect::ws::server::WsServer;

/// Simplified client communicator - just wraps WsServer for broadcast/unicast.
/// All channel routing is done directly in the routing_loop (receiver/mod.rs).
pub(crate) struct ClientCommunicator {
    server: WsServer,
}

/// Route an incoming WS command to the appropriate service.
pub(crate) enum DispatchedCommand {
    Session {
        socket_id: u32,
        command: String,
        payload: serde_json::Value,
    },
    Playback {
        command: String,
        payload: serde_json::Value,
    },
    Queue {
        socket_id: u32,
        command: String,
        payload: serde_json::Value,
    },
    /// Dropped unanswered, as TIDAL's receiver drops it: sent by a socket outside the live
    /// session, which an answer would tell what the session is doing, or one of the messages
    /// TIDAL's receiver sends its own speaker and never takes from a phone.
    Dropped { command: String },
}

/// Every socket may start, resume or end a session, the session being what admits a socket.
/// Everything else is its members' alone.
pub(crate) fn classify_command(
    socket_id: u32,
    command: &str,
    payload: serde_json::Value,
    admission: Admission,
) -> DispatchedCommand {
    match (command, admission) {
        ("startSession" | "resumeSession" | "endSession", _) => DispatchedCommand::Session {
            socket_id,
            command: command.to_string(),
            payload,
        },
        ("requestResources" | "releaseResources", _) | (_, Admission::Outsider) => {
            DispatchedCommand::Dropped {
                command: command.to_string(),
            }
        }
        ("play" | "pause" | "stop" | "seek", Admission::Member) => DispatchedCommand::Playback {
            command: command.to_string(),
            payload,
        },
        (_, Admission::Member) => DispatchedCommand::Queue {
            socket_id,
            command: command.to_string(),
            payload,
        },
    }
}

impl ClientCommunicator {
    pub fn new(server: WsServer) -> Self {
        Self { server }
    }

    pub async fn broadcast(&self, msg: &serde_json::Value) {
        let cmd = msg.get("command").and_then(|v| v.as_str()).unwrap_or("?");
        crate::vprintln!("[connect::ws] -> broadcast: {}", cmd);
        self.server.broadcast(msg).await;
    }

    pub async fn send_to(&self, socket_id: u32, msg: &serde_json::Value) -> anyhow::Result<()> {
        let cmd = msg.get("command").and_then(|v| v.as_str()).unwrap_or("?");
        crate::vprintln!("[connect::ws] -> unicast({}): {}", socket_id, cmd);
        self.server.send_to(socket_id, msg).await
    }

    pub async fn shutdown(&mut self) {
        self.server.shutdown().await;
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/connect/receiver/client_comm.rs"]
mod tests;
