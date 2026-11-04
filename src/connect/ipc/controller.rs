//! IPC handlers for controller-side operations:
//! discovery, device listing, connection lifecycle, auth injection.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::app_state::{IpcCallback, IpcMessage, with_state};
use crate::connect::ConnectManager;
use crate::connect::controller::TidalConnectController;
use crate::connect::controller::dial::{ConnectFailure, RESOLVE_DEADLINE, dial_requested};
use crate::connect::types::{MdnsDevice, ReceiverConfig};
use crate::connect::ws::client::WsClientEvent;

use super::helpers::{
    emit_connect_failed, emit_controller_session_event, forward_controller_notification,
};

/// One latch per operation the handlers retry: the controller init (shared by `discover`,
/// `refresh` and `connect`), the discovery start and the device refresh. Told apart by operation,
/// not cause: their errors are untyped `anyhow::Error`s (mdns-sd, the missing runtime, a task spawn).
static INIT_FAILURE: FailureLatch = FailureLatch::new();
static DISCOVERY_FAILURE: FailureLatch = FailureLatch::new();
static REFRESH_FAILURE: FailureLatch = FailureLatch::new();

fn init_if_missing(cm: &mut ConnectManager) {
    if cm.controller().is_some() {
        return;
    }
    match cm.init_controller() {
        Ok(()) => INIT_FAILURE.recovered(),
        Err(e) => INIT_FAILURE.failed(format_args!("[connect::ipc] Controller init failed: {e}")),
    }
}

fn note_discovery(started: anyhow::Result<()>) {
    match started {
        Ok(()) => DISCOVERY_FAILURE.recovered(),
        Err(e) => {
            DISCOVERY_FAILURE.failed(format_args!("[connect::ipc] Discovery start failed: {e}"));
        }
    }
}

pub(super) fn discover() {
    with_state(|state| {
        if let Some(ref mut cm) = state.connect {
            // Built here on first use: nothing earlier starts mDNS at boot for everyone.
            init_if_missing(cm);
            if let Some(ctrl) = cm.controller() {
                let mut guard = ctrl.lock().unwrap_or_else(|e| e.into_inner());
                note_discovery(guard.start_discovery());
            }
        }
    });
    // Also start receiver if not already running (TIDAL may not call
    // remoteDesktop.initialize explicitly - ensure we're discoverable). The
    // task is a no-op when the receiver is already active.
    if let Some(rt) = crate::state::RT_HANDLE.get() {
        rt.spawn(super::start_receiver_task(ReceiverConfig::default()));
    }
}

pub(super) fn refresh() {
    with_state(|state| {
        if let Some(ref mut cm) = state.connect {
            init_if_missing(cm);
            if let Some(ctrl) = cm.controller() {
                let mut guard = ctrl.lock().unwrap_or_else(|e| e.into_inner());
                match guard.refresh_devices() {
                    Ok(()) => REFRESH_FAILURE.recovered(),
                    Err(e) => REFRESH_FAILURE
                        .failed(format_args!("[connect::ipc] Device refresh failed: {e}")),
                }
            }
        }
    });
}

/// Only the account id is read: the token a device is given comes from `token_state` at send
/// time, never from an argument a plugin can also send.
pub(super) fn set_auth(msg: &IpcMessage) {
    let credential = msg.arg(0).to_string();
    with_state(|state| {
        if let Some(ref mut cm) = state.connect {
            cm.set_session_credential(&credential);
        }
    });
    crate::vprintln!("[connect::ipc] set_auth stored");
}

/// The controller, built and browsing. TIDAL resumes a cast at boot with the device it persisted,
/// before anything asked to discover: that resume is what starts the browse.
fn controller_browsing() -> Option<Arc<Mutex<TidalConnectController>>> {
    with_state(|state| {
        let cm = state.connect.as_mut()?;
        init_if_missing(cm);
        let ctrl = cm.controller()?.clone();
        note_discovery(
            ctrl.lock()
                .unwrap_or_else(|e| e.into_inner())
                .ensure_discovery(),
        );
        Some(ctrl)
    })
    .flatten()
}

/// Every failed connect ends here: logged, announced to TIDAL as CONNECT_TO_DEVICE_FAILED (what
/// releases its queue restore and drops the device it persisted), then answered to the caller.
fn fail_connect(callback: &IpcCallback, failure: &ConnectFailure) {
    let message = failure.message();
    crate::verr!("[connect::ipc] Connect failed: {message}");
    emit_connect_failed();
    callback
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .failure(failure.status(), &message);
}

pub(super) fn connect(msg: IpcMessage, callback: IpcCallback) {
    let device_json = msg.args.first().cloned().unwrap_or_default();
    let Some(rt) = crate::state::RT_HANDLE.get() else {
        fail_connect(&callback, &ConnectFailure::NoRuntime);
        return;
    };
    let ctrl_arc = controller_browsing();
    rt.spawn(async move {
        let (event_tx, mut event_rx) = mpsc::channel::<WsClientEvent>(64);
        let dialled: Result<_, ConnectFailure> = async {
            let ctrl = ctrl_arc.ok_or(ConnectFailure::NoController)?;
            let requested = serde_json::from_value::<MdnsDevice>(device_json)
                .map_err(|e| ConnectFailure::Malformed(e.to_string()))?;
            let (ws, device) =
                dial_requested(&ctrl, &requested, RESOLVE_DEADLINE, false, event_tx).await?;
            Ok((ctrl, ws, device))
        }
        .await;

        match dialled {
            Ok((ctrl, ws, device)) => {
                let ws = Arc::new(ws);
                let (conn_gen, request) = {
                    let mut guard = ctrl.lock().unwrap_or_else(|e| e.into_inner());
                    let cred = guard.session_credential().map(|s| s.to_string());
                    let conn_gen = guard.set_ws_and_start_session(ws, &device, cred.as_deref());
                    (conn_gen, guard.start_session(&device, cred.as_deref()))
                };
                // TIDAL reads `joined` off this answer (`'joined' in t`), which throws on a bare string.
                let joined = request.is_some_and(|r| r.joined());
                callback
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .success_str(&serde_json::json!({ "joined": joined }).to_string());

                // Spawn WS event loop.
                let ctrl_clone = ctrl.clone();
                tokio::spawn(async move {
                    while let Some(event) = event_rx.recv().await {
                        // Checked before the forward below, which pushes raw JSON to the
                        // renderer without consulting the session; a guard further in would
                        // still leak a superseded connection's frames. Breaking rather than
                        // skipping, the generation only ever advancing. One take serves both
                        // the check and the handler, nothing between them awaiting.
                        let session_event = {
                            let mut guard = ctrl_clone.lock().unwrap_or_else(|e| e.into_inner());
                            if guard.connection_gen() != conn_gen {
                                break;
                            }
                            guard.handle_ws_event(&event)
                        };
                        if let WsClientEvent::Message(ref json) = event {
                            forward_controller_notification(json);
                        }
                        if let Some(se) = session_event {
                            emit_controller_session_event(&se);
                        }
                        if matches!(event, WsClientEvent::ConnectionLost) {
                            break;
                        }
                    }
                });
            }
            Err(failure) => fail_connect(&callback, &failure),
        }
    });
}

pub(super) fn disconnect(msg: IpcMessage, callback: IpcCallback) {
    let stop = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(false);
    let session_event = with_state(|state| {
        if let Some(ref cm) = state.connect
            && let Some(ctrl) = cm.controller()
        {
            let mut guard = ctrl.lock().unwrap_or_else(|e| e.into_inner());
            return guard.disconnect(stop);
        }
        None
    })
    .flatten();
    // Outside both locks, and from here rather than the controller, on the same footing as the
    // event loop above. Retiring the session is the only notice the renderer gets, the device's
    // own reply having been overtaken by the teardown that provoked it.
    if let Some(se) = session_event {
        emit_controller_session_event(&se);
    }
    callback
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .success_str("\"ok\"");
}
