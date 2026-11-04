//! Shared helpers for the IPC layer.
//!
//! Covers four kinds of functionality:
//! * State lookups into `ConnectManager` (`get_session_ws`,
//!   `get_last_player_state`, `get_server_infos`).
//! * Device command dispatch (`send_device_cmd`, `send_device_cmd_simple`).
//! * CEF UI-thread posting for IPC event emission (`post_emit`,
//!   `post_emit_with_data`), plus forwarding of controller WS notifications
//!   and session events to the frontend.

use std::sync::Arc;
use std::time::Duration;

use cef::*;

use crate::app_state::{IpcCallback, with_state};
use crate::connect::controller::server_info::{build_content_server_info, build_server_info};
use crate::connect::controller::session::ControllerSessionEvent;
use crate::connect::types::{PlayerState, ServerInfo};
use crate::connect::ws::client::WsClient;
use crate::platform::secure_store::StoredTokenState;

pub(super) const CMD_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn get_last_player_state() -> PlayerState {
    with_state(|state| {
        state.connect.as_ref().and_then(|cm| {
            let ctrl = cm.controller()?;
            let guard = ctrl.lock().unwrap_or_else(|e| e.into_inner());
            Some(guard.last_player_state())
        })
    })
    .flatten()
    .unwrap_or(PlayerState::Idle)
}

pub(super) fn get_session_ws() -> Option<Arc<WsClient>> {
    with_state(|state| {
        state.connect.as_ref().and_then(|cm| {
            let ctrl = cm.controller()?;
            let guard = ctrl.lock().unwrap_or_else(|e| e.into_inner());
            guard.ws_client()
        })
    })
    .flatten()
}

/// Build (content, queue) ServerInfo pair for a given audio quality, `None` when signed out.
/// The token is read here from the store the refreshes rotate, at the moment it is sent.
pub(super) fn get_server_infos(quality: &str) -> Option<(ServerInfo, ServerInfo)> {
    with_state(|state| server_infos_for(state.token_state.as_ref(), quality)).flatten()
}

/// The pair built from the live token alone: no argument carries a token a renderer sent.
fn server_infos_for(
    token_state: Option<&StoredTokenState>,
    quality: &str,
) -> Option<(ServerInfo, ServerInfo)> {
    let access_token = &token_state?.current.access_token;
    Some((
        build_content_server_info(quality, access_token),
        build_server_info("queue", access_token),
    ))
}

pub(super) fn send_device_cmd_simple(command: &'static str, callback: IpcCallback) {
    send_device_cmd(serde_json::json!({"command": command}), callback);
}

pub(super) fn send_device_cmd(cmd: serde_json::Value, callback: IpcCallback) {
    if let Some(rt) = crate::state::RT_HANDLE.get() {
        rt.spawn(async move {
            let Some(ws) = get_session_ws() else {
                callback
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .failure(500, "Not connected");
                return;
            };
            match ws.send_command(cmd, CMD_TIMEOUT).await {
                Ok(response) => {
                    let json = serde_json::to_string(&response).unwrap_or_default();
                    callback
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .success_str(&json);
                }
                Err(e) => {
                    callback
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .failure(500, &e.to_string());
                }
            }
        });
    } else {
        callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failure(500, "No runtime");
    }
}

pub(super) fn forward_controller_notification(json: &serde_json::Value) {
    let cmd = json.get("command").and_then(|v| v.as_str()).unwrap_or("");
    let channel = match cmd {
        "notifyMediaChanged" => "connect.media_changed",
        "notifyPlayerStatusChanged" => "connect.player_status_changed",
        "notifyDeviceStatusChanged" => "connect.device_status_changed",
        "notifyQueueChanged" => "connect.queue_changed",
        "notifyQueueItemsChanged" => "connect.queue_items_changed",
        "notifyContentServerError" | "notifyQueueServerError" => "connect.server_error",
        _ => return,
    };
    post_emit_with_data(channel, json);
}

/// TIDAL's `TCnotifyConnectFailed`: the frontend relays it as CONNECT_TO_DEVICE_FAILED.
pub(super) fn emit_connect_failed() {
    post_emit("connect.connect_failed");
}

pub(super) fn emit_controller_session_event(event: &ControllerSessionEvent) {
    match event {
        ControllerSessionEvent::SessionStarted {
            session_id,
            device,
            joined,
        } => {
            post_emit_with_data(
                "connect.session_started",
                &serde_json::json!({
                    "connectedDevice": device, "sessionId": session_id, "joined": joined,
                }),
            );
        }
        ControllerSessionEvent::SessionEnded { suspended } => {
            post_emit_with_data(
                "connect.session_ended",
                &serde_json::json!({ "suspended": suspended }),
            );
        }
        ControllerSessionEvent::ConnectionLost { during_start } => {
            post_emit("connect.connection_lost");
            // TIDAL's queue restore waits for a session or a failure, not for a lost connection.
            if *during_start {
                emit_connect_failed();
            }
        }
        ControllerSessionEvent::SessionError { command, details } => {
            // A refused startSession is a connect that failed after the dial.
            if command == "startSession" {
                emit_connect_failed();
            }
            post_emit_with_data(
                "connect.server_error",
                &serde_json::json!({
                    "command": command, "details": details,
                }),
            );
        }
    }
}

// `emit_ipc_event*` calls `frame.execute_java_script`, which must run on
// the CEF UI thread. Since connect events originate in tokio tasks, we
// serialise the JS string and post it via `wrap_task!`/`post_task`.

fn build_emit_js(channel: &str, data_json: Option<&str>) -> String {
    let chan = crate::app_state::js_string_literal(channel);
    match data_json {
        Some(json) => format!(
            "if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__({chan},{json});"
        ),
        None => format!(
            "if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__({chan});"
        ),
    }
}

pub(crate) fn post_emit(channel: &str) {
    post_js_to_ui(build_emit_js(channel, None));
}

pub(crate) fn post_emit_with_data(channel: &str, data: &impl serde::Serialize) {
    let json = match serde_json::to_string(data) {
        Ok(j) => crate::app_state::escape_js_line_terminators(j),
        Err(_) => return,
    };
    post_js_to_ui(build_emit_js(channel, Some(&json)));
}

pub(crate) fn post_js_to_ui(js: String) {
    let mut task = ConnectEmitTask::new(js);
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct ConnectEmitTask {
        js: String,
    }
    impl Task {
        fn execute(&self) {
            crate::app_state::eval_js(&self.js);
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/connect/ipc/helpers.rs"]
mod tests;
