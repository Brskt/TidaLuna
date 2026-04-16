//! IPC handlers for controller-side operations:
//! discovery, device listing, connection lifecycle, auth injection.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::app_state::{IpcCallback, IpcMessage, with_state};
use crate::connect::types::{MdnsDevice, ReceiverConfig};
use crate::connect::ws::client::{WsClient, WsClientEvent};

use super::helpers::{
    emit_controller_session_event, forward_controller_notification, get_controller_arc,
};

pub(super) fn initialize(msg: &IpcMessage) {
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

pub(super) fn discover() {
    with_state(|state| {
        if let Some(ref mut cm) = state.connect {
            // Lazy init: discover may arrive before initialize.
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
    // remoteDesktop.initialize explicitly — ensure we're discoverable).
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

pub(super) fn refresh() {
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

pub(super) fn set_auth(msg: &IpcMessage) {
    let credential = msg.arg(0).to_string();
    let token = msg.arg(1).to_string();
    let refresh_tok = msg.arg(2).to_string();
    with_state(|state| {
        if let Some(ref cm) = state.connect
            && let Some(ctrl) = cm.controller()
        {
            let mut guard = ctrl.lock().unwrap();
            guard.set_auth(&credential, &token, &refresh_tok);
        }
    });
    crate::vprintln!("[connect::ipc] set_auth stored");
}

pub(super) fn connect(msg: IpcMessage, callback: IpcCallback) {
    let device_json = msg.args.first().cloned().unwrap_or_default();
    let Some(rt) = crate::state::RT_HANDLE.get() else {
        callback.lock().unwrap().failure(500, "No runtime");
        return;
    };
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
            Some(ref address) => WsClient::connect(address, port, false, event_tx).await,
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

                // Spawn WS event loop.
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

pub(super) fn disconnect(msg: IpcMessage, callback: IpcCallback) {
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
