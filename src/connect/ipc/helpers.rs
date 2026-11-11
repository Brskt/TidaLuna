//! Shared helpers for the IPC layer.
//!
//! Covers four kinds of functionality:
//! * State lookups into `ConnectManager` (`existing_controller`, `get_session_ws`,
//!   `get_last_player_state`).
//! * Device command dispatch (`send_device_cmd`, `send_device_cmd_simple`).
//! * CEF UI-thread posting for IPC event emission (`post_emit`,
//!   `post_emit_with_data`), plus forwarding of controller WS notifications
//!   and session events to the frontend.
//! * Server-info handouts for a device (`prepare_update`, `prepare_load`, `push_server_info`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use cef::*;

use crate::app_state::{IpcCallback, with_state};
use crate::connect::controller::TidalConnectController;
use crate::connect::controller::server_info::{
    Delivery, Queued, ServerInfoClaim, build_content_server_info, build_server_info,
};
use crate::connect::controller::session::ControllerSessionEvent;
use crate::connect::types::{PlayerState, ServerInfo};
use crate::connect::ws::client::WsClient;
use crate::ipc::plugin::{ipc_callback_err, ipc_callback_ok};
use crate::platform::secure_store::StoredTokenState;

use crate::connect::failure_latch::FailureLatch;

pub(super) const CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// The controller if one was built, without building it.
pub(super) fn existing_controller() -> Option<Arc<Mutex<TidalConnectController>>> {
    with_state(|state| {
        state
            .connect
            .as_ref()
            .and_then(|cm| cm.controller())
            .cloned()
    })
    .flatten()
}

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

/// What TIDAL's `serverInfo` holds before a quality is set: the content info carries none.
fn session_infos_for(
    token_state: Option<&StoredTokenState>,
    quality: Option<&str>,
) -> Option<(ServerInfo, ServerInfo)> {
    match quality {
        Some(quality) => server_infos_for(token_state, quality),
        None => {
            let access_token = &token_state?.current.access_token;
            Some((
                build_server_info("content", access_token),
                build_server_info("queue", access_token),
            ))
        }
    }
}

/// The server info a device is to be handed now, with what became of sending it.
pub(super) struct Handout {
    /// The `updateServerInfo` that carries it, for a caller to send itself when no live session took it.
    pub(super) command: serde_json::Value,
    pub(super) due: ServerInfoClaim,
}

fn update_command(content: &ServerInfo, queue: &ServerInfo) -> serde_json::Value {
    serde_json::json!({
        "command": "updateServerInfo",
        "contentServerInfo": content,
        "queueServerInfo": queue,
    })
}

/// Hands the live session's device the server info for the live token and the session quality
/// (`set_quality` first, when TIDAL names one that counts); `None` when signed out. The token is
/// read here, from the store the refreshes rotate.
pub(super) fn prepare_update(set_quality: Option<&str>) -> Option<Handout> {
    with_state(|state| {
        // Remembered ahead of the token: a quality named before the first sign-in still reaches
        // the device along with it.
        let quality = match state.connect.as_mut() {
            Some(cm) => {
                cm.remember_quality(set_quality);
                cm.session_quality().map(str::to_owned)
            }
            None => set_quality.filter(|q| !q.is_empty()).map(str::to_owned),
        };
        let token_state = state.token_state.as_ref()?;
        let access_token = &token_state.current.access_token;
        let (content, queue) = session_infos_for(Some(token_state), quality.as_deref())?;
        let command = update_command(&content, &queue);
        let due = match state.connect.as_ref().and_then(|cm| cm.controller()) {
            Some(ctrl) => deliver(ctrl, access_token, quality.as_deref(), command.clone()),
            None => ServerInfoClaim::NoSession,
        };
        Some(Handout { command, due })
    })
    .flatten()
}

fn deliver(
    ctrl: &Mutex<TidalConnectController>,
    token: &str,
    quality: Option<&str>,
    command: serde_json::Value,
) -> ServerInfoClaim {
    ctrl.lock()
        .unwrap_or_else(|e| e.into_inner())
        .deliver_server_info(token, quality, command)
}

/// What a `loadCloudQueue` carries: the quality TIDAL named, if it named one that counts,
/// is remembered; the pair is built with `sent`, which is never remembered. No delivery recorded.
pub(super) fn prepare_load(named: Option<&str>, sent: &str) -> Option<(ServerInfo, ServerInfo)> {
    with_state(|state| {
        if let Some(cm) = state.connect.as_mut() {
            cm.remember_quality(named);
        }
        server_infos_for(state.token_state.as_ref(), sent)
    })
    .flatten()
}

/// The answer of a queued delivery, which the controller takes back when none comes.
async fn await_delivery(
    ctrl: &Mutex<TidalConnectController>,
    queued: Queued,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    match queued.reply.wait(timeout).await {
        Ok(response) => Ok(response),
        Err(e) => {
            ctrl.lock()
                .unwrap_or_else(|e| e.into_inner())
                .release_server_info(&queued.delivery);
            Err(e.to_string())
        }
    }
}

/// `await_delivery` on the app's controller.
async fn await_queued(queued: Queued) -> Result<serde_json::Value, String> {
    let ctrl = existing_controller().ok_or_else(|| "Not connected".to_string())?;
    await_delivery(&ctrl, queued, CMD_TIMEOUT).await
}

/// One report per outage of the token push no renderer call waits on.
static PUSH_FAILURE: FailureLatch = FailureLatch::new();

/// Follows the token a fire-and-forget `setAuthentication` owed its device: the frame is queued
/// already, what is left is to take the delivery back if the device never answers.
pub(super) fn push_server_info(due: ServerInfoClaim) {
    let queued = match due {
        ServerInfoClaim::Queued(queued) => queued,
        ServerInfoClaim::Failed(e) => {
            PUSH_FAILURE.failed(format_args!("[connect::ipc] Token push failed: {e}"));
            return;
        }
        ServerInfoClaim::NoSession | ServerInfoClaim::Held => return,
    };
    let Some(rt) = crate::state::RT_HANDLE.get() else {
        release_delivery(&queued.delivery);
        return;
    };
    rt.spawn(async move {
        match await_queued(queued).await {
            Ok(_) => PUSH_FAILURE.recovered(),
            Err(e) => PUSH_FAILURE.failed(format_args!("[connect::ipc] Token push failed: {e}")),
        }
    });
}

/// `update_quality`'s answer for a frame queued: the caller waits on `callback`.
pub(super) fn answer_queued(queued: Queued, callback: IpcCallback) {
    let Some(rt) = crate::state::RT_HANDLE.get() else {
        release_delivery(&queued.delivery);
        ipc_callback_err(&callback, 500, "No runtime");
        return;
    };
    rt.spawn(async move {
        let answer = match await_queued(queued).await {
            Ok(response) => serde_json::to_string(&response).unwrap_or_default(),
            Err(e) => {
                ipc_callback_err(&callback, 500, &e);
                return;
            }
        };
        ipc_callback_ok(&callback, &answer);
    });
}

/// Takes back a delivery nothing was sent for; left recorded, it would skip the next as a repeat.
fn release_delivery(delivery: &Delivery) {
    with_state(|state| {
        if let Some(ctrl) = state.connect.as_ref().and_then(|cm| cm.controller()) {
            ctrl.lock()
                .unwrap_or_else(|e| e.into_inner())
                .release_server_info(delivery);
        }
    });
}

pub(super) fn send_device_cmd_simple(command: &'static str, callback: IpcCallback) {
    send_device_cmd(serde_json::json!({"command": command}), callback);
}

async fn run_device_cmd(cmd: serde_json::Value) -> Result<serde_json::Value, String> {
    let ws = get_session_ws().ok_or_else(|| "Not connected".to_string())?;
    ws.send_command(cmd, CMD_TIMEOUT)
        .await
        .map_err(|e| e.to_string())
}

pub(super) fn send_device_cmd(cmd: serde_json::Value, callback: IpcCallback) {
    if let Some(rt) = crate::state::RT_HANDLE.get() {
        rt.spawn(async move {
            match run_device_cmd(cmd).await {
                Ok(response) => {
                    let json = serde_json::to_string(&response).unwrap_or_default();
                    ipc_callback_ok(&callback, &json);
                }
                Err(e) => ipc_callback_err(&callback, 500, &e),
            }
        });
    } else {
        ipc_callback_err(&callback, 500, "No runtime");
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

/// `generation` is the controller's socket generation read in the lock hold that produced `event`:
/// the page drops a lifecycle event older than one it applied, as a push queued before its
/// snapshot read lands behind the snapshot's reply.
pub(super) fn emit_controller_session_event(event: &ControllerSessionEvent, generation: u64) {
    match event {
        ControllerSessionEvent::SessionStarted {
            session_id,
            device,
            joined,
            connection,
        } => {
            post_emit_with_data(
                "connect.session_started",
                &serde_json::json!({
                    "connectedDevice": device, "sessionId": session_id, "joined": joined,
                    "connection": connection,
                }),
            );
        }
        ControllerSessionEvent::SessionEnded { suspended } => {
            post_emit_with_data(
                "connect.session_ended",
                &serde_json::json!({ "suspended": suspended, "connection": generation }),
            );
        }
        ControllerSessionEvent::ConnectionLost => {
            post_emit_with_data(
                "connect.connection_lost",
                &serde_json::json!({ "connection": generation }),
            );
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
