pub(crate) mod dial;
pub(crate) mod flight;
pub(crate) mod server_info;
pub(crate) mod session;

use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::connect::mdns::browser::{BrowserEvent, MdnsBrowser};
use crate::connect::types::{
    MdnsDevice, MediaInfo, PlaybackNotification, PlayerState, SessionNotification,
};
use crate::connect::ws::client::{WsClient, WsClientEvent};

use session::{ControllerSession, ControllerSessionEvent, SessionRequest};

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
        }
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

    /// Called after WsClient::connect succeeds (from IPC handler).
    /// Stores the WS client and starts/resumes a session.
    ///
    /// Returns the installed connection's generation, which the caller must hand to the event
    /// loop it spawns; that is what lets a superseded socket's events be discarded.
    pub fn set_ws_and_start_session(
        &mut self,
        ws: Arc<WsClient>,
        device: &MdnsDevice,
        _credential: Option<&str>,
    ) -> u64 {
        // Bumped before either branch shuts an old socket down: the terminal event that
        // shutdown provokes must already be from a stale generation by the time it lands.
        self.connection_gen = self.connection_gen.wrapping_add(1);
        // Close old WS if switching device
        if let Some(ref mut old_session) = self.session {
            if old_session
                .connected_device()
                .is_some_and(|d| d.id == device.id)
            {
                // Same device reconnect: replace the dead WS and keep session state,
                // letting queue_start_or_resume send resumeSession on the new socket.
                old_session.ws.shutdown();
                old_session.ws = ws;
                return self.connection_gen;
            }
            // Different device - close old connection
            old_session.ws.shutdown();
        }
        let session = ControllerSession::new(ws);
        self.session = Some(session);
        self.connection_gen
    }

    /// Start or resume the session (sync part - the WS send is fire-and-forget).
    /// Returns which of the two was queued, `None` when there is no session or the command did
    /// not serialize.
    pub fn start_session(
        &mut self,
        device: &MdnsDevice,
        credential: Option<&str>,
    ) -> Option<SessionRequest> {
        // Fire the start/resume command (async send, but send_raw is non-blocking via channel)
        self.session
            .as_mut()?
            .queue_start_or_resume(device, credential)
            .ok()
    }

    /// Disconnect from current device (sync - queues the endSession command).
    ///
    /// Returns the event the caller must emit when this call retires the session. The device's
    /// own `notifySessionEnded` cannot carry that announcement; `queue_end` only hands the
    /// command to a spawned send. The teardown below runs first and takes away both the
    /// session the reply would be matched against and the currency of the loop delivering it.
    /// Returned rather than emitted, as [`handle_ws_event`](Self::handle_ws_event) does it:
    /// this layer speaks the protocol, the IPC layer above talks to the renderer.
    #[must_use = "dropping the event leaves the renderer believing it is still connected"]
    pub fn disconnect(&mut self, stop_casting: bool) -> Option<ControllerSessionEvent> {
        if let Some(ref mut session) = self.session {
            let _ = session.queue_end(stop_casting);
        }
        if !stop_casting {
            return None;
        }
        // Retiring the session retires its socket's event loop with it. Nothing here
        // closes the socket; absent the bump, that loop stays current and goes on
        // forwarding frames from a device the user has just disconnected from.
        self.connection_gen = self.connection_gen.wrapping_add(1);
        // `suspended: false` is what the device itself would answer, `stop_casting` being the
        // protocol's way of saying "ended, not suspended". Nothing to announce without a
        // session, the renderer never having been told it was connected.
        self.session
            .take()
            .map(|_| ControllerSessionEvent::SessionEnded { suspended: false })
    }

    /// Handle a WS client event (message, connection lost, error).
    pub fn handle_ws_event(&mut self, event: &WsClientEvent) -> Option<ControllerSessionEvent> {
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
                            return session.handle_notification(&notification);
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

    pub fn session_info(&self) -> Option<(&str, &MdnsDevice, bool)> {
        self.session.as_ref().and_then(|s| s.session_info())
    }

    pub fn is_connected(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.ws.is_connected())
    }

    /// Whether TIDAL is told that `failure` ended a connect. Not while a session stays live: its
    /// CONNECT_TO_DEVICE_FAILED reads as "not connected" and drops the persisted device, which
    /// would leave a live session the renderer believes gone. TIDAL disconnects before it
    /// connects, so only a plugin's direct request reaches this with a session up.
    pub fn announces_connect_failure(&self, failure: &dial::ConnectFailure) -> bool {
        failure.is_announced() && !self.is_connected()
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
