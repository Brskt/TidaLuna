//! Shared helpers for the IPC layer.
//!
//! Covers three kinds of functionality:
//! * State lookups into `ConnectManager` (`get_controller_arc`,
//!   `get_session_ws`, `get_last_player_state`, `get_server_infos`).
//! * Device command dispatch (`send_device_cmd`, `send_device_cmd_simple`).
//! * CEF UI-thread posting for IPC event emission (`post_emit`,
//!   `post_emit_with_data`), plus forwarding of controller WS notifications
//!   and session events to the frontend.

use std::sync::Arc;
use std::time::Duration;

use cef::*;
use tokio::sync::Mutex as TokioMutex;

use crate::app_state::{IpcCallback, with_state};
use crate::connect::controller::session::ControllerSessionEvent;
use crate::connect::types::{PlayerState, ServerInfo};
use crate::connect::ws::client::WsClient;

pub(super) const CMD_TIMEOUT: Duration = Duration::from_secs(10);

// ── State lookups ────────────────────────────────────────────────────

pub(super) fn get_controller_arc()
-> Option<Arc<std::sync::Mutex<crate::connect::controller::TidalConnectController>>> {
    with_state(|state| {
        state
            .connect
            .as_ref()
            .and_then(|cm| cm.controller().cloned())
    })
    .flatten()
}

pub(super) fn get_last_player_state() -> PlayerState {
    with_state(|state| {
        state.connect.as_ref().and_then(|cm| {
            let ctrl = cm.controller()?;
            let guard = ctrl.lock().ok()?;
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
            let guard = ctrl.lock().ok()?;
            guard.ws_client()
        })
    })
    .flatten()
}

/// Build (content, queue) ServerInfo pair from controller state for a
/// given audio quality.
pub(super) fn get_server_infos(quality: &str) -> Option<(ServerInfo, ServerInfo)> {
    with_state(|state| {
        state.connect.as_ref().and_then(|cm| {
            let ctrl = cm.controller()?;
            let guard = ctrl.lock().ok()?;
            Some((
                guard.build_content_server_info(quality),
                guard.build_server_info("queue"),
            ))
        })
    })
    .flatten()
}

// ── Device command dispatch ──────────────────────────────────────────

pub(super) fn send_device_cmd_simple(command: &'static str, callback: IpcCallback) {
    send_device_cmd(serde_json::json!({"command": command}), callback);
}

pub(super) fn send_device_cmd(cmd: serde_json::Value, callback: IpcCallback) {
    if let Some(rt) = crate::state::RT_HANDLE.get() {
        rt.spawn(async move {
            let Some(ws) = get_session_ws() else {
                callback.lock().unwrap().failure(500, "Not connected");
                return;
            };
            match ws.send_command(cmd, CMD_TIMEOUT).await {
                Ok(response) => {
                    let json = serde_json::to_string(&response).unwrap_or_default();
                    callback.lock().unwrap().success_str(&format!("S:{json}"));
                }
                Err(e) => {
                    callback.lock().unwrap().failure(500, &e.to_string());
                }
            }
        });
    } else {
        callback.lock().unwrap().failure(500, "No runtime");
    }
    // Silence unused warning when dependencies shift.
    let _ = TokioMutex::<()>::new(());
}

// ── Event forwarding (controller → frontend) ─────────────────────────

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
        ControllerSessionEvent::ConnectionLost => {
            post_emit("connect.connection_lost");
        }
        ControllerSessionEvent::SessionError { command, details } => {
            post_emit_with_data(
                "connect.server_error",
                &serde_json::json!({
                    "command": command, "details": details,
                }),
            );
        }
    }
}

// ── CEF UI-thread posting ────────────────────────────────────────────
// `emit_ipc_event*` calls `frame.execute_java_script`, which must run on
// the CEF UI thread. Since connect events originate in tokio tasks, we
// serialise the JS string and post it via `wrap_task!`/`post_task`.

pub(crate) fn post_emit(channel: &str) {
    let js = format!(
        "if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__('{}');",
        channel.replace('\'', "\\'")
    );
    post_js_to_ui(js);
}

pub(crate) fn post_emit_with_data(channel: &str, data: &impl serde::Serialize) {
    let json = match serde_json::to_string(data) {
        Ok(j) => j,
        Err(_) => return,
    };
    let js = format!(
        "if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__('{}',{json});",
        channel.replace('\'', "\\'")
    );
    post_js_to_ui(js);
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
