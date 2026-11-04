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

/// Whether TIDAL is told `failure` ended a connect, no controller meaning no session to protect.
fn announces_failure(
    ctrl: Option<&Arc<Mutex<TidalConnectController>>>,
    failure: &ConnectFailure,
) -> bool {
    ctrl.is_none_or(|ctrl| {
        ctrl.lock()
            .unwrap_or_else(|e| e.into_inner())
            .announces_connect_failure(failure)
    })
}

/// A failure answered without an attempt, or by a leader that died. Announced to TIDAL as
/// CONNECT_TO_DEVICE_FAILED when `announce`: that releases its queue restore and drops the
/// device it persisted.
fn fail_connect(callback: &IpcCallback, failure: &ConnectFailure, announce: bool) {
    log_failure(failure);
    if announce {
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
        Err(failure) => Verdict {
            announce: announces_failure(ctrl, &failure),
            result: Err(failure),
        },
    }
}

/// A replaced attempt closes what it dialled: nothing else holds the socket.
fn discard_dialled(dialled: Dialled) {
    dialled.ws.shutdown();
}

/// Retires the socket an install put in place unless the install and the event loop reading it
/// finish: a panic, or a task dropped mid-way, would leave it open with nobody reading.
struct InstallGuard {
    ctrl: Arc<Mutex<TidalConnectController>>,
    conn_gen: Option<u64>,
    armed: bool,
}

impl InstallGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        let (true, Some(conn_gen)) = (self.armed, self.conn_gen) else {
            return;
        };
        let mut guard = self.ctrl.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(se) = guard.abandon_install(conn_gen) {
            emit_controller_session_event(&se, guard.connection_gen());
        }
    }
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
    // Declared before the controller lock below: an unwind releases that lock first.
    let mut undo = InstallGuard {
        ctrl: ctrl.clone(),
        conn_gen: None,
        armed: true,
    };
    let (conn_gen, request) = {
        let mut guard = ctrl.lock().unwrap_or_else(|e| e.into_inner());
        let cred = guard.session_credential().map(|s| s.to_string());
        let installed = guard.set_ws_and_start_session(ws, &device, cred.as_deref());
        undo.conn_gen = Some(installed.conn_gen);
        // Posted in the hold that bumped the generation, where the old loop's own posts are
        // made: its last event cannot land behind this one.
        if let Some(se) = installed.superseded {
            emit_controller_session_event(&se, installed.conn_gen);
        }
        (
            installed.conn_gen,
            guard.start_session(&device, cred.as_deref()),
        )
    };

    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            // The check, the handler and every post to the renderer share one hold of the lock
            // that bumps the generation: a supersede lands wholly before or after them, so no
            // event of a superseded socket is posted behind that supersede's own `session_ended`.
            // Breaking rather than skipping, the generation only ever advancing; the posts only
            // queue a task on the UI thread.
            {
                let mut guard = ctrl.lock().unwrap_or_else(|e| e.into_inner());
                if guard.connection_gen() != conn_gen {
                    break;
                }
                let session_event = guard.handle_ws_event(&event);
                if let WsClientEvent::Message(ref json) = event {
                    forward_controller_notification(json);
                }
                if let Some(se) = session_event {
                    emit_controller_session_event(&se, guard.connection_gen());
                }
            }
            if matches!(event, WsClientEvent::ConnectionLost) {
                break;
            }
        }
        undo.disarm();
    });
    request.is_some_and(|r| r.joined())
}

pub(super) fn connect(msg: IpcMessage, callback: IpcCallback) {
    let device_json = msg.args.first().cloned().unwrap_or_default();
    let Some(rt) = crate::state::RT_HANDLE.get() else {
        // Flights run on that runtime: without it, none can be in progress to protect.
        fail_connect(&callback, &ConnectFailure::NoRuntime, true);
        return;
    };
    let ctrl_arc = controller_browsing();
    rt.spawn(async move {
        let requested = match serde_json::from_value::<MdnsDevice>(device_json) {
            Ok(requested) => requested,
            Err(e) => {
                let failure = ConnectFailure::Malformed(e.to_string());
                let announce = announces_failure(ctrl_arc.as_ref(), &failure);
                fail_connect(&callback, &failure, announce);
                return;
            }
        };
        let fullname = requested.fullname.clone();
        // Also what the exit runs for every request still pending (`close_connect_flights`):
        // TIDAL is told of the failure once, by the leader.
        let abort = {
            let (callback, ctrl) = (callback.clone(), ctrl_arc.clone());
            move |led: bool| {
                let failure = ConnectFailure::Aborted;
                let announce = led && announces_failure(ctrl.as_ref(), &failure);
                fail_connect(&callback, &failure, announce);
            }
        };
        let mut settled = CONNECT_FLIGHTS
            .run(
                &fullname,
                dial_device(ctrl_arc.clone(), requested),
                |dialled| settle_attempt(ctrl_arc.as_ref(), dialled),
                discard_dialled,
                abort,
            )
            .await;
        // The exit may have answered this request already.
        if !settled.claim() {
            return;
        }
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

/// Answers every connect request still pending, while the CEF side they answer through is alive:
/// a later drop of their tasks has nothing left to run. Called from `ConnectManager::shutdown`.
pub(crate) fn close_connect_flights() {
    CONNECT_FLIGHTS.close();
}

pub(super) fn disconnect(msg: IpcMessage, callback: IpcCallback) {
    let stop = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(false);
    with_state(|state| {
        if let Some(ref cm) = state.connect
            && let Some(ctrl) = cm.controller()
        {
            let mut guard = ctrl.lock().unwrap_or_else(|e| e.into_inner());
            // From here rather than the controller, in the hold that bumped the generation, as
            // the event loop does. Retiring the session is the only notice the renderer gets, the
            // device's own reply having been overtaken by the teardown that provoked it.
            if let Some(se) = guard.disconnect(stop) {
                emit_controller_session_event(&se, guard.connection_gen());
            }
        }
    });
    callback
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .success_str("\"ok\"");
}
