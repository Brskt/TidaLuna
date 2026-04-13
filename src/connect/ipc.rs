use std::sync::Arc;
use std::time::Duration;

use crate::app_state::{IpcCallback, IpcMessage, with_state};
use crate::connect::controller::session::ControllerSessionEvent;
use crate::connect::types::{MdnsDevice, PlayerState, ReceiverConfig};
use crate::connect::ws::client::{WsClient, WsClientEvent};
use cef::*;
use tokio::sync::mpsc;

const CMD_TIMEOUT: Duration = Duration::from_secs(10);

// ── Fire-and-forget IPC (no callback) ────────────────────────────────

pub(crate) fn handle_connect_ipc(msg: &IpcMessage) {
    let sub = msg.channel.strip_prefix("connect.").unwrap_or("");
    crate::vprintln!("[connect::ipc] {}", sub);

    match sub {
        "controller.initialize" => {
            let queue_url = msg.arg(1).to_string();
            let content_url = msg.arg(2).to_string();
            let auth_url = msg.arg(3).to_string();
            with_state(|state| {
                if let Some(ref mut cm) = state.connect {
                    if let Err(e) = cm.init_controller() {
                        crate::vprintln!("[connect::ipc] Controller init failed: {}", e);
                    }
                    if let Some(ctrl) = cm.controller() {
                        let mut guard = ctrl.lock().unwrap();
                        guard.set_server_urls(&queue_url, &content_url, &auth_url);
                    }
                }
            });
        }
        "controller.discover" => {
            with_state(|state| {
                if let Some(ref mut cm) = state.connect {
                    // Lazy init: discover may arrive before initialize
                    if cm.controller().is_none() {
                        let _ = cm.init_controller();
                    }
                    if let Some(ctrl) = cm.controller() {
                        let mut guard = ctrl.lock().unwrap();
                        let _ = guard.start_discovery();
                    }
                }
            });
            // Also start receiver if not already running (TIDAL may not call
            // remoteDesktop.initialize explicitly - ensure we're discoverable)
            if let Some(rt) = crate::state::RT_HANDLE.get() {
                rt.spawn(async move {
                    let mut cm = with_state(|state| state.connect.take()).flatten();
                    if let Some(ref mut cm) = cm
                        && !cm.is_receiver_active()
                    {
                        let config = ReceiverConfig::default();
                        if let Err(e) = cm.start_receiver(config).await {
                            crate::vprintln!("[connect::ipc] Receiver auto-start failed: {}", e);
                        }
                    }
                    with_state(|state| {
                        state.connect = cm;
                    });
                });
            }
        }
        "controller.refresh" => {
            with_state(|state| {
                if let Some(ref mut cm) = state.connect {
                    if cm.controller().is_none() {
                        let _ = cm.init_controller();
                    }
                    if let Some(ctrl) = cm.controller() {
                        let mut guard = ctrl.lock().unwrap();
                        let _ = guard.refresh_devices();
                    }
                }
            });
        }
        "controller.set_auth" => {
            let credential = msg.arg(0).to_string();
            let token = msg.arg(1).to_string();
            let refresh = msg.arg(2).to_string();
            with_state(|state| {
                if let Some(ref cm) = state.connect
                    && let Some(ctrl) = cm.controller()
                {
                    let mut guard = ctrl.lock().unwrap();
                    guard.set_auth(&credential, &token, &refresh);
                }
            });
            crate::vprintln!("[connect::ipc] set_auth stored");
        }
        "receiver.start" => {
            let config = ReceiverConfig::default();
            if let Some(rt) = crate::state::RT_HANDLE.get() {
                rt.spawn(async move {
                    let mut cm = with_state(|state| state.connect.take()).flatten();
                    if let Some(ref mut cm) = cm
                        && let Err(e) = cm.start_receiver(config).await
                    {
                        crate::vprintln!("[connect::ipc] Receiver start failed: {}", e);
                    }
                    with_state(|state| {
                        state.connect = cm;
                    });
                });
            }
        }
        "receiver.stop" => {
            if let Some(rt) = crate::state::RT_HANDLE.get() {
                rt.spawn(async {
                    let mut cm = with_state(|state| state.connect.take()).flatten();
                    if let Some(ref mut cm) = cm {
                        cm.stop_receiver().await;
                    }
                    with_state(|state| {
                        state.connect = cm;
                    });
                });
            }
        }
        _ => {
            crate::vprintln!("[connect::ipc] Unknown fire-and-forget: connect.{}", sub);
        }
    }
}

// ── Invoke IPC (with callback) ───────────────────────────────────────

pub(crate) fn handle_connect_invoke(msg: IpcMessage, callback: IpcCallback) {
    let sub = msg
        .channel
        .strip_prefix("connect.")
        .unwrap_or("")
        .to_string();

    match sub.as_str() {
        // ── State query ──────────────────────────────────────────
        "get_state" => {
            let snapshot =
                with_state(|state| state.connect.as_ref().map(|cm| cm.get_state_snapshot()))
                    .flatten()
                    .unwrap_or(serde_json::json!({}));
            callback.lock().unwrap().success_str(&format!(
                "S:{}",
                serde_json::to_string(&snapshot).unwrap_or_default()
            ));
        }

        // ── Connect to device ────────────────────────────────────
        "controller.connect" => {
            let device_json = msg.args.first().cloned().unwrap_or_default();
            if let Some(rt) = crate::state::RT_HANDLE.get() {
                rt.spawn(async move {
                    let ctrl_arc = get_controller_arc();
                    let Some(ctrl) = ctrl_arc else {
                        callback.lock().unwrap().failure(500, "No controller");
                        return;
                    };

                    let device: MdnsDevice = match serde_json::from_value(device_json) {
                        Ok(d) => d,
                        Err(e) => {
                            callback.lock().unwrap().failure(400, &e.to_string());
                            return;
                        }
                    };

                    let (event_tx, mut event_rx) = mpsc::channel::<WsClientEvent>(64);
                    let addr = device.addresses.first().cloned();
                    let port = device.port;

                    let ws = match addr {
                        Some(ref address) => {
                            WsClient::connect(address, port, false, event_tx).await
                        }
                        None => Err(anyhow::anyhow!("Device has no address")),
                    };

                    match ws {
                        Ok(ws) => {
                            let ws = Arc::new(ws);
                            {
                                let mut guard = ctrl.lock().unwrap();
                                let cred = guard.session_credential().map(|s| s.to_string());
                                guard.set_ws_and_start_session(ws, &device, cred.as_deref());
                                guard.start_session(&device, cred.as_deref());
                            }
                            callback.lock().unwrap().success_str("S:\"ok\"");

                            // Spawn WS event loop
                            let ctrl_clone = ctrl.clone();
                            tokio::spawn(async move {
                                while let Some(event) = event_rx.recv().await {
                                    if let WsClientEvent::Message(ref json) = event {
                                        forward_controller_notification(json);
                                    }
                                    let session_event = {
                                        let mut guard = ctrl_clone.lock().unwrap();
                                        guard.handle_ws_event(&event)
                                    };
                                    if let Some(se) = session_event {
                                        emit_controller_session_event(&se);
                                    }
                                    if matches!(event, WsClientEvent::ConnectionLost) {
                                        break;
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            callback.lock().unwrap().failure(500, &e.to_string());
                        }
                    }
                });
            }
        }

        // ── Disconnect ───────────────────────────────────────────
        "controller.disconnect" => {
            let stop = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(false);
            with_state(|state| {
                if let Some(ref cm) = state.connect
                    && let Some(ctrl) = cm.controller()
                {
                    let mut guard = ctrl.lock().unwrap();
                    guard.disconnect(stop);
                }
            });
            callback.lock().unwrap().success_str("S:\"ok\"");
        }

        // ── Controller commands → send to device via WS ──────────
        // Correct wire names from mediaManager.js / queueManager.js
        "controller.play_or_pause" => {
            let cmd = match get_last_player_state() {
                PlayerState::Playing => "pause",
                _ => "play",
            };
            send_device_cmd_simple(cmd, callback);
        }
        "controller.play_next" => send_device_cmd_simple("next", callback),
        "controller.play_previous" => send_device_cmd_simple("previous", callback),
        "controller.refresh_queue" => send_device_cmd_simple("refreshQueue", callback),

        "controller.seek" => {
            let position = msg.args.first().and_then(|v| v.as_u64()).unwrap_or(0);
            send_device_cmd(
                serde_json::json!({"command": "seek", "position": position}),
                callback,
            );
        }
        "controller.set_volume" => {
            let level = msg.args.first().and_then(|v| v.as_f64()).unwrap_or(0.0);
            send_device_cmd(
                serde_json::json!({"command": "setVolume", "level": level}),
                callback,
            );
        }
        "controller.set_mute" => {
            let mute = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(false);
            send_device_cmd(
                serde_json::json!({"command": "setMute", "mute": mute}),
                callback,
            );
        }
        "controller.set_repeat" => {
            let mode = msg
                .args
                .first()
                .and_then(|v| v.as_str())
                .unwrap_or("NONE")
                .to_string();
            send_device_cmd(
                serde_json::json!({"command": "setRepeatMode", "repeatMode": mode}),
                callback,
            );
        }
        "controller.set_shuffle" => {
            let shuffle = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(false);
            send_device_cmd(
                serde_json::json!({"command": "setShuffle", "shuffle": shuffle}),
                callback,
            );
        }
        "controller.load_media" => {
            let media_info = msg.args.first().cloned().unwrap_or_default();
            send_device_cmd(
                serde_json::json!({"command": "loadMediaInfo", "mediaInfo": media_info}),
                callback,
            );
        }
        "controller.load_queue" => {
            let data = msg.args.first().cloned().unwrap_or_default();
            let quality = data
                .get("audioquality")
                .and_then(|v| v.as_str())
                .unwrap_or("HIGH");
            let Some((content_si, queue_si)) = get_server_infos(quality) else {
                callback.lock().unwrap().failure(500, "No controller");
                return;
            };

            let cmd = serde_json::json!({
                "command": "loadCloudQueue",
                "autoplay": data.get("autoplay").and_then(|v| v.as_bool()).unwrap_or(false),
                "position": data.get("position").and_then(|v| v.as_u64()).unwrap_or(0),
                "currentMediaInfo": data.get("currentMediaInfo"),
                "queueInfo": {
                    "queueId": data.get("queueId").and_then(|v| v.as_str()).unwrap_or(""),
                    "repeatMode": data.get("repeatMode").and_then(|v| v.as_str()).unwrap_or("NONE"),
                    "shuffled": data.get("shuffled").and_then(|v| v.as_bool()).unwrap_or(false),
                    "maxAfterSize": data.get("maxAfterSize").and_then(|v| v.as_u64()).unwrap_or(10),
                    "maxBeforeSize": data.get("maxBeforeSize").and_then(|v| v.as_u64()).unwrap_or(10),
                },
                "contentServerInfo": content_si,
                "queueServerInfo": queue_si,
            });
            send_device_cmd(cmd, callback);
        }
        "controller.select_queue_item" => {
            let media_info = msg.args.first().cloned().unwrap_or_default();
            send_device_cmd(
                serde_json::json!({"command": "selectQueueItem", "mediaInfo": media_info}),
                callback,
            );
        }
        "controller.update_quality" => {
            let quality = msg
                .args
                .first()
                .and_then(|v| v.as_str())
                .unwrap_or("HIGH")
                .to_string();
            let Some((content_si, queue_si)) = get_server_infos(&quality) else {
                callback.lock().unwrap().failure(500, "No controller");
                return;
            };

            send_device_cmd(
                serde_json::json!({
                    "command": "updateServerInfo",
                    "contentServerInfo": content_si,
                    "queueServerInfo": queue_si,
                }),
                callback,
            );
        }

        _ => {
            // Fallback: try fire-and-forget path
            handle_connect_ipc(&msg);
            callback.lock().unwrap().success_str("ok");
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

fn get_controller_arc()
-> Option<Arc<std::sync::Mutex<crate::connect::controller::TidalConnectController>>> {
    with_state(|state| {
        state
            .connect
            .as_ref()
            .and_then(|cm| cm.controller().cloned())
    })
    .flatten()
}

fn get_last_player_state() -> PlayerState {
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

fn get_session_ws() -> Option<Arc<WsClient>> {
    with_state(|state| {
        state.connect.as_ref().and_then(|cm| {
            let ctrl = cm.controller()?;
            let guard = ctrl.lock().ok()?;
            guard.ws_client()
        })
    })
    .flatten()
}

use crate::connect::types::ServerInfo;

/// Build content + queue ServerInfo pair from controller state.
fn get_server_infos(quality: &str) -> Option<(ServerInfo, ServerInfo)> {
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

/// Send a command with no extra fields.
fn send_device_cmd_simple(command: &'static str, callback: IpcCallback) {
    send_device_cmd(serde_json::json!({"command": command}), callback);
}

/// Send a JSON command to the device via WS send_command (with requestId + timeout).
fn send_device_cmd(cmd: serde_json::Value, callback: IpcCallback) {
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
}

fn forward_controller_notification(json: &serde_json::Value) {
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

fn emit_controller_session_event(event: &ControllerSessionEvent) {
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

// ── CEF UI thread posting ───────────────────────────────────────────
// emit_ipc_event* functions call frame.execute_java_script which must run
// on the CEF UI thread. Since connect events originate from tokio tasks,
// we serialize the JS string and post it via wrap_task!/post_task.

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
