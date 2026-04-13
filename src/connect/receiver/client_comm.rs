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
}

pub(crate) fn classify_command(
    socket_id: u32,
    command: &str,
    payload: serde_json::Value,
) -> DispatchedCommand {
    match command {
        "startSession" | "resumeSession" | "endSession" | "releaseResources"
        | "requestResources" => DispatchedCommand::Session {
            socket_id,
            command: command.to_string(),
            payload,
        },
        "play" | "pause" | "stop" | "seek" => DispatchedCommand::Playback {
            command: command.to_string(),
            payload,
        },
        _ => DispatchedCommand::Queue {
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
