//! IPC handlers for controller-side operations:
//! discovery, device listing, connection lifecycle, auth injection.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::app_state::{IpcCallback, IpcMessage, with_state};
use crate::connect::ConnectManager;
use crate::connect::controller::TidalConnectController;
use crate::connect::controller::dial::{
    ConnectFailure, RESOLVE_DEADLINE, RESUME_DEADLINE, Started, confirm_resume, confirm_start,
    dial_requested,
};
use crate::connect::controller::flight::{ConnectFlights, Verdict};
use crate::connect::controller::session::{ControllerSessionEvent, ResumePlan};
use crate::connect::failure_latch::FailureLatch;
use crate::connect::types::{
    DeviceText, MdnsDevice, ReceiverConfig, TextField, normalize_device_frame,
};
use crate::connect::ws::client::{WsClient, WsClientEvent};

use super::helpers::{
    emit_connect_failed, emit_controller_session_event, forward_controller_notification,
    prepare_update, push_server_info,
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
/// time, never from an argument a plugin can also send. A live session's device is handed the
/// token held now, unless it holds it already; with none live, the next install hands it over.
pub(super) fn set_auth(msg: &IpcMessage) {
    let credential = msg.arg(0).to_string();
    with_state(|state| {
        if let Some(ref mut cm) = state.connect {
            cm.set_session_credential(&credential);
        }
    });
    crate::vprintln!("[connect::ipc] set_auth stored");
    if let Some(handout) = prepare_update(None) {
        push_server_info(handout.due);
    }
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

/// The controller if one was built, without building it.
fn existing_controller() -> Option<Arc<Mutex<TidalConnectController>>> {
    with_state(|state| {
        state
            .connect
            .as_ref()
            .and_then(|cm| cm.controller())
            .cloned()
    })
    .flatten()
}

/// One per process: every connect request, whoever sent it, goes through the same attempt.
static CONNECT_FLIGHTS: ConnectFlights = ConnectFlights::new();

fn log_failure(failure: &ConnectFailure) {
    let message = failure.message();
    if matches!(
        failure,
        ConnectFailure::Superseded | ConnectFailure::Cancelled
    ) {
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

/// What a connect needs of the link to the device, fixed for the real one and narrowed by tests.
struct Link {
    disable_tls: bool,
    resume_deadline: Duration,
}

const TIDAL_LINK: Link = Link {
    disable_tls: false,
    resume_deadline: RESUME_DEADLINE,
};

/// A session the device confirmed resuming on the new socket, with what the wait read.
struct Resumed {
    plan: ResumePlan,
    frames: Vec<WsClientEvent>,
}

/// What the device confirmed on the new socket: a session resumed, or one started.
enum Established {
    Resumed(Resumed),
    Started(Started),
}

/// What the dial produced: the socket to install, the device it reached and what it confirmed.
struct Dialled {
    ctrl: Arc<Mutex<TidalConnectController>>,
    ws: WsClient,
    device: MdnsDevice,
    events: mpsc::Receiver<WsClientEvent>,
    established: Established,
}

/// Closes the socket it holds unless taken: a dial replaced or cancelled while it waits for the
/// device drops its future there, and nothing else holds the socket.
struct Unsettled(Option<WsClient>);

impl Unsettled {
    fn ws(&self) -> &WsClient {
        self.0.as_ref().expect("held until taken")
    }

    fn take(mut self) -> WsClient {
        self.0.take().expect("held until taken")
    }
}

impl Drop for Unsettled {
    fn drop(&mut self) {
        if let Some(ws) = self.0.take() {
            ws.shutdown();
        }
    }
}

/// The part of the attempt that awaits: resolve the device, dial it and have it confirm on the new
/// socket the resume of its session, or a start when it has none. Nothing is installed, and a
/// session keeps its old socket until the settle step.
async fn dial_device(
    ctrl: Option<Arc<Mutex<TidalConnectController>>>,
    requested: MdnsDevice,
    link: &Link,
) -> Result<Dialled, ConnectFailure> {
    let (event_tx, mut events) = mpsc::channel::<WsClientEvent>(64);
    let ctrl = ctrl.ok_or(ConnectFailure::NoController)?;
    // Taken before the dial: a session the device or the user ends meanwhile is not resumed
    // into a new one by a request that asked for the old.
    let plan = ctrl
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .resume_plan(&requested.fullname);
    let (ws, device) = dial_requested(
        &ctrl,
        &requested,
        RESOLVE_DEADLINE,
        link.disable_tls,
        event_tx,
    )
    .await?;
    let held = Unsettled(Some(ws));
    let established = match plan {
        Some(plan) => {
            let frames = confirm_resume(
                held.ws(),
                &mut events,
                &plan.session_id,
                link.resume_deadline,
            )
            .await?;
            Established::Resumed(Resumed { plan, frames })
        }
        None => Established::Started(confirm_start(held.ws(), &mut events, &ctrl).await?),
    };
    Ok(Dialled {
        ctrl,
        ws: held.take(),
        device,
        events,
        established,
    })
}

/// Runs under the flights lock, while the attempt is still the current one: lock order is flights,
/// then controller.
fn settle_attempt(
    ctrl: Option<&Arc<Mutex<TidalConnectController>>>,
    dialled: Result<Dialled, ConnectFailure>,
) -> Verdict {
    match dialled.and_then(install_session) {
        Ok(joined) => Verdict::done(joined),
        Err(failure) => Verdict {
            announce: settle_failure(ctrl, &failure, &Renderer),
            result: Err(failure),
        },
    }
}

/// An announced failure first ends the session whose socket is no longer live, through the end a
/// disconnect announces: `connect_failed` alone would leave TIDAL on the remote player. A session
/// TIDAL was told ended owes nothing. Whether TIDAL is then told the connect failed.
fn settle_failure(
    ctrl: Option<&Arc<Mutex<TidalConnectController>>>,
    failure: &ConnectFailure,
    page: &impl Page,
) -> bool {
    // No controller means no session to protect.
    let Some(ctrl) = ctrl else {
        return true;
    };
    let mut guard = ctrl.lock().unwrap_or_else(|e| e.into_inner());
    if failure.is_announced()
        && let Some(se) = guard.end_unless_live()
    {
        page.session_event(&se, guard.connection_gen());
    }
    guard.announces_connect_failure(failure)
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

/// Install the session the device confirmed and watch its socket. Returns the `joined` the device
/// reported, or why nothing was installed: the session the request was to resume is gone.
fn install_session(dialled: Dialled) -> Result<bool, ConnectFailure> {
    let Dialled {
        ctrl,
        ws,
        device,
        events: event_rx,
        established,
    } = dialled;
    let ws = Arc::new(ws);
    // Declared before the controller lock below: an unwind releases that lock first.
    let mut undo = InstallGuard {
        ctrl: ctrl.clone(),
        conn_gen: None,
        armed: true,
    };
    let (conn_gen, joined, replay, token_owed) = {
        let mut guard = ctrl.lock().unwrap_or_else(|e| e.into_inner());
        let (adopted, joined, replay) = match established {
            Established::Resumed(Resumed { plan, frames }) => {
                (guard.adopt_resumed(ws, &device, &plan)?, true, frames)
            }
            Established::Started(Started {
                session_id,
                joined,
                frames,
            }) => (
                guard.adopt_started(ws, &device, session_id, joined),
                joined,
                frames,
            ),
        };
        undo.conn_gen = Some(adopted.conn_gen);
        // Posted in the hold that bumped the generation, by this task and ahead of its answer: the
        // loop below is another task, and the old loop's own posts cannot land behind these.
        for se in [adopted.superseded, adopted.started].into_iter().flatten() {
            emit_controller_session_event(&se, adopted.conn_gen);
        }
        (adopted.conn_gen, joined, replay, adopted.token_owed)
    };
    if token_owed && let Some(handout) = prepare_update(None) {
        push_server_info(handout.due);
    }

    tokio::spawn(async move {
        read_connection(&ctrl, conn_gen, replay, event_rx, &Renderer).await;
        undo.disarm();
    });
    Ok(joined)
}

/// Relays what the device sends on connection `conn_gen` until that connection is over: first the
/// frames the resume wait read, which the session has not seen, then what arrives after them.
async fn read_connection(
    ctrl: &Mutex<TidalConnectController>,
    conn_gen: u64,
    replay: Vec<WsClientEvent>,
    mut events: mpsc::Receiver<WsClientEvent>,
    page: &impl Page,
) {
    for event in replay {
        if !relay_event(ctrl, conn_gen, event, page) {
            return;
        }
    }
    while let Some(event) = events.recv().await {
        if !relay_event(ctrl, conn_gen, event, page) {
            return;
        }
    }
}

/// What the page is told of a connection's events: the renderer, or a recorder under test.
trait Page {
    fn notification(&self, frame: &serde_json::Value);
    fn session_event(&self, event: &ControllerSessionEvent, generation: u64);
}

struct Renderer;

impl Page for Renderer {
    fn notification(&self, frame: &serde_json::Value) {
        forward_controller_notification(frame);
    }

    fn session_event(&self, event: &ControllerSessionEvent, generation: u64) {
        emit_controller_session_event(event, generation);
    }
}

/// Applies one event of connection `conn_gen` and posts it; whether that connection's loop goes on.
fn relay_event(
    ctrl: &Mutex<TidalConnectController>,
    conn_gen: u64,
    event: WsClientEvent,
    page: &impl Page,
) -> bool {
    // The check, the handler and every post to the page share one hold of the lock that bumps the
    // generation: no event of a superseded socket lands behind its supersede's `session_ended`. A
    // stale event ends the loop (the generation only advances); the posts only queue a UI task.
    let mut guard = ctrl.lock().unwrap_or_else(|e| e.into_inner());
    if guard.connection_gen() != conn_gen {
        return false;
    }
    // Parsed once, past that check and ahead of the controller and the page: both read one frame.
    // TIDAL's own controller throws on a non-JSON `description` and the page never hears the frame;
    // a non-JSON `customData` costs the track display, and the frame goes on without the field.
    let mut relay = true;
    let event = match event {
        WsClientEvent::Message(frame) => {
            let (frame, text) = normalize_device_frame(frame);
            if let Some((field, state)) = text {
                let latch = guard.frame_faults().latch(field);
                match state {
                    TextField::NotJson(fault) => {
                        latch.failed(format_args!("[connect::controller] {fault}"));
                        relay = field != DeviceText::Description;
                    }
                    TextField::Parsed => latch.recovered(),
                    TextField::Untouched => {}
                }
            }
            WsClientEvent::Message(frame)
        }
        other => other,
    };
    let session_event = guard.handle_ws_event(&event);
    if relay && let WsClientEvent::Message(ref json) = event {
        page.notification(json);
    }
    if let Some(se) = session_event {
        page.session_event(&se, guard.connection_gen());
    }
    !matches!(event, WsClientEvent::ConnectionLost)
}

pub(super) fn connect(msg: IpcMessage, callback: IpcCallback) {
    let device_json = msg.args.first().cloned().unwrap_or_default();
    let Some(rt) = crate::state::RT_HANDLE.get() else {
        let failure = ConnectFailure::NoRuntime;
        let announce = settle_failure(existing_controller().as_ref(), &failure, &Renderer);
        fail_connect(&callback, &failure, announce);
        return;
    };
    let ctrl_arc = controller_browsing();
    rt.spawn(async move {
        let requested = match serde_json::from_value::<MdnsDevice>(device_json) {
            Ok(requested) => requested,
            Err(e) => {
                let failure = ConnectFailure::Malformed(e.to_string());
                let announce = settle_failure(ctrl_arc.as_ref(), &failure, &Renderer);
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
                let announce = led && settle_failure(ctrl.as_ref(), &failure, &Renderer);
                fail_connect(&callback, &failure, announce);
            }
        };
        let mut settled = CONNECT_FLIGHTS
            .run(
                &fullname,
                dial_device(ctrl_arc.clone(), requested, &TIDAL_LINK),
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

/// Ends the connect still dialling, then the session. The attempt goes first, flights being ahead
/// of the controller in the lock order: a leader that settles after it installs nothing.
fn end_session(
    flights: &ConnectFlights,
    ctrl: &Mutex<TidalConnectController>,
    stop_casting: bool,
    page: &impl Page,
) {
    flights.cancel();
    let mut guard = ctrl.lock().unwrap_or_else(|e| e.into_inner());
    // From here rather than the controller, in the hold that bumped the generation, as the event
    // loop does. Retiring the session is the only notice the renderer gets, the device's own reply
    // having been overtaken by the teardown that provoked it.
    if let Some(se) = guard.disconnect(stop_casting) {
        page.session_event(&se, guard.connection_gen());
    }
}

pub(super) fn disconnect(msg: IpcMessage, callback: IpcCallback) {
    let stop = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(false);
    if let Some(ctrl) = existing_controller() {
        end_session(&CONNECT_FLIGHTS, &ctrl, stop, &Renderer);
    }
    callback
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .success_str("\"ok\"");
}

#[cfg(test)]
#[path = "../../../tests/unit/connect/ipc/controller.rs"]
mod tests;
