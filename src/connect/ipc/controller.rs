//! IPC handlers for controller-side operations:
//! discovery, device listing, connection lifecycle, auth injection.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::app_state::{IpcCallback, IpcMessage, with_state};
use crate::connect::ConnectManager;
use crate::connect::controller::TidalConnectController;
use crate::connect::controller::dial::{ConnectFailure, RESOLVE_DEADLINE, dial_requested};
use crate::connect::controller::flight::{ConnectFlights, Verdict};
use crate::connect::types::{MdnsDevice, ReceiverConfig};
use crate::connect::ws::client::{WsClient, WsClientEvent};

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

/// One per process: every connect request, whoever sent it, goes through the same attempt.
static CONNECT_FLIGHTS: ConnectFlights = ConnectFlights::new();

fn log_failure(failure: &ConnectFailure) {
    let message = failure.message();
    if matches!(failure, ConnectFailure::Superseded) {
        crate::vprintln!("[connect::ipc] Connect superseded: {message}");
    } else {
        crate::verr!("[connect::ipc] Connect failed: {message}");
    }
}

fn answer_failure(callback: &IpcCallback, failure: &ConnectFailure) {
    callback
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .failure(failure.status(), &failure.message());
}

/// A failure that never reached an attempt. Announced to TIDAL as CONNECT_TO_DEVICE_FAILED, what
/// releases its queue restore and drops the device it persisted, unless it names no device. Only
/// `NoRuntime` gets here announced: a flight runs on that runtime, so none can be in progress.
fn fail_connect(callback: &IpcCallback, failure: &ConnectFailure) {
    log_failure(failure);
    if failure.is_announced() {
        emit_connect_failed();
    }
    answer_failure(callback, failure);
}

/// What the dial produced: the socket to install and the device it reached.
struct Dialled {
    ctrl: Arc<Mutex<TidalConnectController>>,
    ws: WsClient,
    device: MdnsDevice,
    events: mpsc::Receiver<WsClientEvent>,
}

/// The part of the attempt that awaits: resolve the device and dial it. Nothing is installed.
async fn dial_device(
    ctrl: Option<Arc<Mutex<TidalConnectController>>>,
    requested: MdnsDevice,
) -> Result<Dialled, ConnectFailure> {
    let (event_tx, events) = mpsc::channel::<WsClientEvent>(64);
    let ctrl = ctrl.ok_or(ConnectFailure::NoController)?;
    let (ws, device) = dial_requested(&ctrl, &requested, RESOLVE_DEADLINE, false, event_tx).await?;
    Ok(Dialled {
        ctrl,
        ws,
        device,
        events,
    })
}

/// Runs under the flights lock, while the attempt is still the current one: lock order is flights,
/// then controller.
fn settle_attempt(
    ctrl: Option<&Arc<Mutex<TidalConnectController>>>,
    dialled: Result<Dialled, ConnectFailure>,
) -> Verdict {
    match dialled {
        Ok(dialled) => Verdict::done(install_session(dialled)),
        Err(failure) => {
            let announce = ctrl.is_none_or(|ctrl| {
                ctrl.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .announces_connect_failure(&failure)
            });
            Verdict {
                result: Err(failure),
                announce,
            }
        }
    }
}

/// A replaced attempt closes what it dialled: nothing else holds the socket.
fn discard_dialled(dialled: Dialled) {
    dialled.ws.shutdown();
}

/// Install the session and watch its socket. Returns whether the device already held our session.
fn install_session(dialled: Dialled) -> bool {
    let Dialled {
        ctrl,
        ws,
        device,
        events: mut event_rx,
    } = dialled;
    let ws = Arc::new(ws);
    let (conn_gen, request) = {
        let mut guard = ctrl.lock().unwrap_or_else(|e| e.into_inner());
        let cred = guard.session_credential().map(|s| s.to_string());
        let conn_gen = guard.set_ws_and_start_session(ws, &device, cred.as_deref());
        (conn_gen, guard.start_session(&device, cred.as_deref()))
    };

    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            // Checked before the forward below, which pushes raw JSON to the
            // renderer without consulting the session; a guard further in would
            // still leak a superseded connection's frames. Breaking rather than
            // skipping, the generation only ever advancing. One take serves both
            // the check and the handler, nothing between them awaiting.
            let session_event = {
                let mut guard = ctrl.lock().unwrap_or_else(|e| e.into_inner());
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
    request.is_some_and(|r| r.joined())
}

pub(super) fn connect(msg: IpcMessage, callback: IpcCallback) {
    let device_json = msg.args.first().cloned().unwrap_or_default();
    let Some(rt) = crate::state::RT_HANDLE.get() else {
        fail_connect(&callback, &ConnectFailure::NoRuntime);
        return;
    };
    let ctrl_arc = controller_browsing();
    rt.spawn(async move {
        let requested = match serde_json::from_value::<MdnsDevice>(device_json) {
            Ok(requested) => requested,
            Err(e) => {
                fail_connect(&callback, &ConnectFailure::Malformed(e.to_string()));
                return;
            }
        };
        let fullname = requested.fullname.clone();
        let settled = CONNECT_FLIGHTS
            .run(
                &fullname,
                dial_device(ctrl_arc.clone(), requested),
                |dialled| settle_attempt(ctrl_arc.as_ref(), dialled),
                discard_dialled,
            )
            .await;
        match &settled.result {
            // TIDAL reads `joined` off this answer (`'joined' in t`), which throws on a bare string.
            Ok(joined) => callback
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .success_str(&serde_json::json!({ "joined": joined }).to_string()),
            Err(failure) => {
                if settled.led() {
                    log_failure(failure);
                }
                if settled.announces_failure() {
                    emit_connect_failed();
                }
                answer_failure(&callback, failure);
            }
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
