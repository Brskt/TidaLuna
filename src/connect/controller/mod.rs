pub(crate) mod dial;
pub(crate) mod flight;
pub(crate) mod server_info;
pub(crate) mod session;

use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::connect::failure_latch::FailureLatch;
use crate::connect::mdns::browser::{BrowserEvent, MdnsBrowser};
use crate::connect::types::{
    DeviceText, MdnsDevice, MediaInfo, PlaybackNotification, PlayerState, SessionNotification,
};
use crate::connect::ws::client::{WsClient, WsClientEvent};

use dial::ConnectFailure;
use server_info::{Queued, ServerInfoClaim, ServerInfoLedger};
use session::{ControllerSession, ControllerSessionEvent, EndQueue, ResumePlan};

/// Cap on tracked discovered devices; a malicious LAN advertiser could otherwise
/// grow the list without bound by announcing many instances.
const MAX_DISCOVERED_DEVICES: usize = 256;

/// Insert `device` into `devices` deduped by `fullname`, refusing to grow past
/// `max` entries.
fn insert_device_capped(devices: &mut Vec<MdnsDevice>, device: MdnsDevice, max: usize) {
    if devices.iter().any(|d| d.fullname == device.fullname) {
        return;
    }
    if devices.len() >= max {
        crate::vprintln!("[connect::controller] Discovery list full; ignoring new device");
        return;
    }
    devices.push(device);
}

/// One report per outage of a device sending frames whose text field is not JSON, a latch for
/// each field: a chatty device cannot spend the budget of the `verr!` lines everything else
/// shares, and one field's outage cannot hide or re-arm the other's.
#[derive(Default)]
pub(crate) struct FrameFaults {
    description: FailureLatch,
    custom_data: FailureLatch,
}

impl FrameFaults {
    pub(crate) fn latch(&self, field: DeviceText) -> &FailureLatch {
        match field {
            DeviceText::Description => &self.description,
            DeviceText::CustomData => &self.custom_data,
        }
    }
}

/// A session installed on the socket the device confirmed it on: the events TIDAL is owed, in order.
#[must_use = "dropping the events leaves the renderer believing the session is not what it is"]
pub(crate) struct Adopted {
    pub(crate) conn_gen: u64,
    /// The end of the live session this one replaced.
    pub(crate) superseded: Option<ControllerSessionEvent>,
    /// The start, unless TIDAL holds this session live already.
    pub(crate) started: Option<ControllerSessionEvent>,
    /// A token was due while no live session could take it: hand over the current one.
    pub(crate) token_owed: bool,
}

/// Controller orchestrator: discovers devices, connects via WSS, manages session.
pub(crate) struct TidalConnectController {
    cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    browser: Option<MdnsBrowser>,
    session: Option<ControllerSession>,
    /// Currently discovered devices (cached for get_state snapshot)
    discovered_devices: Vec<MdnsDevice>,
    /// Bumped by every browser event applied to the list: what a connect request waits on
    /// for a device the browse has not resolved yet.
    device_changes: watch::Sender<()>,
    /// Last known player state from notifyPlayerStatusChanged
    last_player_state: PlayerState,
    /// Last known playback progress (ms)
    last_progress: u64,
    /// Last known media info from notifyMediaChanged
    last_media: Option<MediaInfo>,
    session_credential: Option<String>,
    /// Bumped on every WS install. `WsClient::shutdown()` emits `ConnectionLost`
    /// unconditionally on its way out: a socket we superseded on purpose looks exactly
    /// like one the network killed and the event carries no identity of its own. Each
    /// per-connection event loop remembers its generation and discards anything older.
    connection_gen: u64,
    /// TIDAL was told a connection started and no end has followed: a loss alone leaves it
    /// owed, its saga waiting on the end of a device it still holds. Moved by `announce` only.
    end_owed: bool,
    /// A handout found no live session to take it: the next install hands over the current token.
    token_owed: bool,
    server_info: ServerInfoLedger,
    frame_faults: FrameFaults,
}

impl TidalConnectController {
    pub fn new(browser: Option<MdnsBrowser>) -> Self {
        Self {
            cancel: CancellationToken::new(),
            tasks: Vec::new(),
            browser,
            session: None,
            discovered_devices: Vec::new(),
            device_changes: watch::channel(()).0,
            last_player_state: PlayerState::Idle,
            last_progress: 0,
            last_media: None,
            session_credential: None,
            connection_gen: 0,
            end_owed: false,
            token_owed: false,
            server_info: ServerInfoLedger::default(),
            frame_faults: FrameFaults::default(),
        }
    }

    /// Every lifecycle event this controller hands out passes here, so that `end_owed` follows
    /// what TIDAL was told: a start owes an end, an end settles it, anything else leaves it.
    fn announce(
        &mut self,
        event: Option<ControllerSessionEvent>,
    ) -> Option<ControllerSessionEvent> {
        match event {
            Some(ControllerSessionEvent::SessionStarted { .. }) => self.end_owed = true,
            Some(ControllerSessionEvent::SessionEnded { .. }) => self.end_owed = false,
            _ => {}
        }
        event
    }

    pub fn connection_gen(&self) -> u64 {
        self.connection_gen
    }

    /// Start mDNS discovery.
    pub fn start_discovery(&mut self) -> anyhow::Result<()> {
        if let Some(ref mut browser) = self.browser {
            browser.start_discovery()?;
        }
        Ok(())
    }

    /// Start mDNS discovery unless a browse is already running: restarting a live one would
    /// only reset its query schedule.
    pub fn ensure_discovery(&mut self) -> anyhow::Result<()> {
        match self.browser {
            Some(ref mut browser) if !browser.is_discovering() => browser.start_discovery(),
            _ => Ok(()),
        }
    }

    pub fn stop_discovery(&mut self) {
        if let Some(ref mut browser) = self.browser {
            browser.stop_discovery();
        }
    }

    pub fn refresh_devices(&mut self) -> anyhow::Result<()> {
        if let Some(ref mut browser) = self.browser {
            browser.refresh()?;
        }
        Ok(())
    }

    /// Installs the session the device started on `ws`, which it confirmed. A session it replaces
    /// closes with its socket, a live one coming back as the end TIDAL is told. The generation
    /// goes to the event loop the caller spawns, which drops a superseded socket's events.
    pub fn adopt_started(
        &mut self,
        ws: Arc<WsClient>,
        device: &MdnsDevice,
        session_id: String,
        joined: bool,
    ) -> Adopted {
        // Bumped before the old socket shuts down: the terminal event that shutdown provokes must
        // already be from a stale generation by the time it lands.
        self.connection_gen = self.connection_gen.wrapping_add(1);
        let conn_gen = self.connection_gen;
        let superseded = self.session.as_mut().and_then(|old| {
            old.ws.shutdown();
            old.end_live()
                .then_some(ControllerSessionEvent::SessionEnded { suspended: false })
        });
        let (session, started) =
            ControllerSession::started(ws, conn_gen, device.clone(), session_id, joined);
        self.session = Some(session);
        // In this order: the start is announced last and leaves the new session owing its end.
        let superseded = self.announce(superseded);
        let started = self.announce(Some(started));
        Adopted {
            conn_gen,
            superseded,
            started,
            token_owed: std::mem::take(&mut self.token_owed),
        }
    }

    /// The plan for moving the session of the device `fullname`, live or kept after a loss or a
    /// suspension, to a new socket; `None` when there is none to move.
    pub fn resume_plan(&self, fullname: &str) -> Option<ResumePlan> {
        self.session.as_ref()?.resume_plan(fullname)
    }

    /// Moves the planned session onto `ws`, the socket the device resumed it on; the old one closes
    /// only now. `Cancelled` when the session ended or moved meanwhile, a resume failure when `ws`
    /// died since. Either way `ws` is closed and the session left as it was.
    pub fn adopt_resumed(
        &mut self,
        ws: Arc<WsClient>,
        device: &MdnsDevice,
        plan: &ResumePlan,
    ) -> Result<Adopted, ConnectFailure> {
        let Some(session) = self.session.as_mut().filter(|s| s.is_planned(plan, device)) else {
            ws.shutdown();
            return Err(ConnectFailure::Cancelled);
        };
        if !ws.is_connected() {
            ws.shutdown();
            return Err(ConnectFailure::Resume(
                "The connection was lost before the session moved onto it".to_string(),
            ));
        }
        // Bumped ahead of the close, as in the other installs: its `ConnectionLost` is stale.
        self.connection_gen = self.connection_gen.wrapping_add(1);
        let conn_gen = self.connection_gen;
        session.ws.shutdown();
        session.ws = ws;
        session.connection = conn_gen;
        let started = session.resumed();
        Ok(Adopted {
            conn_gen,
            superseded: None,
            started: self.announce(started),
            token_owed: std::mem::take(&mut self.token_owed),
        })
    }

    /// A connect that failed leaves a live session on its socket. Otherwise nothing is left to
    /// resume: the session is retired and the end TIDAL is owed comes back, as `disconnect` does.
    pub fn end_unless_live(&mut self) -> Option<ControllerSessionEvent> {
        if self.is_connected() {
            return None;
        }
        self.retire_socket(false);
        self.announce_owed_end()
    }

    /// The end of a session TIDAL was told started, when no end has been announced since.
    fn announce_owed_end(&mut self) -> Option<ControllerSessionEvent> {
        let owed = self
            .end_owed
            .then_some(ControllerSessionEvent::SessionEnded { suspended: false });
        self.announce(owed)
    }

    /// Undo an install that did not complete: close the socket and drop its session, unless a newer
    /// install already took over. Returns the event that tells TIDAL, when the session was live.
    pub fn abandon_install(&mut self, conn_gen: u64) -> Option<ControllerSessionEvent> {
        if self.connection_gen != conn_gen {
            return None;
        }
        let was_live = self
            .session
            .as_mut()
            .is_some_and(ControllerSession::end_live);
        self.retire_socket(false);
        self.announce(was_live.then_some(ControllerSessionEvent::SessionEnded { suspended: false }))
    }

    /// Disconnect from current device (sync - queues the endSession command).
    ///
    /// Returns the event the caller must emit when `stop_casting` leaves an end owed. The device's
    /// own `notifySessionEnded` cannot carry that announcement: the teardown below runs first and
    /// takes away both the session the reply would be matched against and the currency of the
    /// loop delivering it.
    /// Returned rather than emitted, as [`handle_ws_event`](Self::handle_ws_event) does it:
    /// this layer speaks the protocol, the IPC layer above talks to the renderer.
    #[must_use = "dropping the event leaves the renderer believing it is still connected"]
    pub fn disconnect(&mut self, stop_casting: bool) -> Option<ControllerSessionEvent> {
        if let Some(ref session) = self.session
            && let EndQueue::Lost(e) = session.queue_end(stop_casting)
        {
            crate::verr!(
                "[connect::controller] endSession (stopCasting={stop_casting}) was not queued, \
                 the device never hears it: {e}"
            );
        }
        if !stop_casting {
            return None;
        }
        // Retiring the session retires its socket's event loop with it: absent the bump, that
        // loop stays current and goes on forwarding frames from a device the user has just
        // disconnected from.
        self.connection_gen = self.connection_gen.wrapping_add(1);
        // `suspended: false` is the device's own answer to `stop_casting` (ended, not suspended).
        // Announced when an end is owed, session or not (TIDAL's connect saga waits on it after a
        // loss or a refused connect), and never twice after the device's own end.
        // The socket closes with its last holder, once the endSession queued above is written.
        self.session = None;
        self.announce_owed_end()
    }

    /// Cuts the socket of a session TIDAL is told is over, dropping what it still queues: the device
    /// ended the session (and closes its side) or the connection is dead. Every connect dials a
    /// new one. `keep_session` holds a suspended session for the resume. The generation moves
    /// first, as in `disconnect`: the cut's `ConnectionLost` is stale.
    fn retire_socket(&mut self, keep_session: bool) {
        self.connection_gen = self.connection_gen.wrapping_add(1);
        if keep_session {
            if let Some(session) = &self.session {
                session.ws.shutdown();
            }
        } else if let Some(session) = self.session.take() {
            session.ws.shutdown();
        }
    }

    /// Handle a WS client event (message, connection lost, error).
    pub fn handle_ws_event(&mut self, event: &WsClientEvent) -> Option<ControllerSessionEvent> {
        let outcome = self.apply_ws_event(event);
        self.announce(outcome)
    }

    fn apply_ws_event(&mut self, event: &WsClientEvent) -> Option<ControllerSessionEvent> {
        match event {
            WsClientEvent::Message(json) => {
                let command = json.get("command").and_then(|v| v.as_str()).unwrap_or("");

                match command {
                    // Session notifications - parse only when relevant
                    "notifySessionStarted"
                    | "notifySessionEnded"
                    | "notifySessionResumed"
                    | "notifySessionError"
                    | "notifySessionState" => {
                        if let Ok(notification) =
                            serde_json::from_value::<SessionNotification>(json.clone())
                            && let Some(ref mut session) = self.session
                        {
                            let event = session.handle_notification(&notification);
                            if let Some(ControllerSessionEvent::SessionEnded { suspended }) = &event
                            {
                                self.retire_socket(*suspended);
                            }
                            return event;
                        }
                    }
                    // Cache playback state for play/pause routing and get_state snapshot
                    "notifyPlayerStatusChanged" => {
                        if let Ok(PlaybackNotification::NotifyPlayerStatusChanged {
                            player_state,
                            progress,
                            ..
                        }) = serde_json::from_value::<PlaybackNotification>(json.clone())
                        {
                            self.last_player_state = player_state;
                            self.last_progress = progress;
                        }
                    }
                    "notifyMediaChanged" => {
                        if let Ok(PlaybackNotification::NotifyMediaChanged { media_info }) =
                            serde_json::from_value::<PlaybackNotification>(json.clone())
                        {
                            self.last_media = Some(*media_info);
                        }
                    }
                    "notifyContentServerError" | "notifyQueueServerError" => {
                        self.server_info.forget_sent();
                    }
                    _ => {}
                }
                None
            }
            WsClientEvent::ConnectionLost => {
                if let Some(ref mut session) = self.session {
                    return session.handle_connection_lost();
                }
                None
            }
            WsClientEvent::Error(e) => {
                crate::vprintln!("[connect::controller] WS error: {}", e);
                None
            }
        }
    }

    /// Handle a browser event (device found/removed).
    /// Uses `fullname` as the dedup/removal key (TXT id != instance name).
    pub fn handle_browser_event(&mut self, event: BrowserEvent) {
        match event {
            BrowserEvent::DeviceFound(device) => {
                insert_device_capped(&mut self.discovered_devices, device, MAX_DISCOVERED_DEVICES);
                // Sort alphabetically by friendly_name (case-insensitive)
                self.discovered_devices.sort_by(|a, b| {
                    a.friendly_name
                        .to_lowercase()
                        .cmp(&b.friendly_name.to_lowercase())
                });
            }
            BrowserEvent::DeviceRemoved { fullname } => {
                self.discovered_devices.retain(|d| d.fullname != fullname);
            }
        }
        self.device_changes.send_replace(());
    }

    /// A receiver woken by every change of the discovered list. Taken in the same lock hold as
    /// the lookup it guards. A device resolved after the lookup cannot be missed.
    pub fn watch_devices(&self) -> watch::Receiver<()> {
        self.device_changes.subscribe()
    }

    pub fn discovered_devices(&self) -> &[MdnsDevice] {
        &self.discovered_devices
    }

    /// The device mDNS found under `fullname`. A connect request names one by this key and gets
    /// its address from here: the renderer, plugins included, never chooses where to dial.
    pub fn discovered_device(&self, fullname: &str) -> Option<MdnsDevice> {
        self.discovered_devices
            .iter()
            .find(|d| d.fullname == fullname)
            .cloned()
    }

    /// Get the WS client from the active session (for sending commands).
    pub fn ws_client(&self) -> Option<Arc<WsClient>> {
        self.session.as_ref().map(|s| s.ws.clone())
    }

    pub fn last_player_state(&self) -> PlayerState {
        self.last_player_state
    }

    pub fn last_progress(&self) -> u64 {
        self.last_progress
    }

    pub fn last_media(&self) -> Option<&MediaInfo> {
        self.last_media.as_ref()
    }

    pub fn session_info(&self) -> Option<(&str, &MdnsDevice, bool, u64)> {
        self.session.as_ref().and_then(|s| s.session_info())
    }

    /// What TIDAL's `isConnected` says: a session it was told started or resumed and not ended
    /// or lost since.
    pub fn is_connected(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(ControllerSession::is_live)
    }

    /// Whether TIDAL is told that `failure` ended a connect. Not while a session stays live: its
    /// CONNECT_TO_DEVICE_FAILED reads as "not connected" and drops the persisted device, which
    /// would leave a live session the renderer believes gone. A session TIDAL was told ended is
    /// not live, and a failed connect then announces.
    pub fn announces_connect_failure(&self, failure: &dial::ConnectFailure) -> bool {
        failure.is_announced() && !self.is_connected()
    }

    /// Hands the live session's device `command` (`updateServerInfo`) unless it holds that token and
    /// quality already. With no live session the next install hands over the token then current.
    /// Queued in the hold that records it: the wire keeps the order, the answer is awaited outside.
    pub fn deliver_server_info(
        &mut self,
        token: &str,
        quality: Option<&str>,
        command: serde_json::Value,
    ) -> ServerInfoClaim {
        let Some(session) = self.session.as_ref().filter(|s| s.is_live()) else {
            self.token_owed = true;
            return ServerInfoClaim::NoSession;
        };
        let Some(delivery) = self.server_info.claim(self.connection_gen, token, quality) else {
            return ServerInfoClaim::Held;
        };
        match session.ws.queue_command(command) {
            Ok(reply) => ServerInfoClaim::Queued(Queued { delivery, reply }),
            Err(e) => {
                self.server_info.release(&delivery);
                ServerInfoClaim::Failed(e.to_string())
            }
        }
    }

    pub fn release_server_info(&mut self, delivery: &server_info::Delivery) {
        self.server_info.release(delivery);
    }

    pub fn frame_faults(&self) -> &FrameFaults {
        &self.frame_faults
    }

    pub fn set_session_credential(&mut self, credential: Option<&str>) {
        self.session_credential = credential.map(str::to_string);
    }

    pub fn session_credential(&self) -> Option<&str> {
        self.session_credential.as_deref()
    }

    pub fn shutdown(&mut self) {
        self.cancel.cancel();
        self.stop_discovery();
        // Bumped ahead of the close: `WsClient::shutdown` provokes a terminal
        // `ConnectionLost` that reads exactly like a real drop, and reporting that to the UI
        // during teardown is the false disconnect this generation exists to suppress.
        self.connection_gen = self.connection_gen.wrapping_add(1);
        // Close WS before dropping session
        if let Some(ref session) = self.session {
            session.ws.shutdown();
        }
        self.session = None;
        for task in self.tasks.drain(..) {
            task.abort();
        }
        crate::vprintln!("[connect::controller] Shut down");
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/connect/controller.rs"]
mod tests;
