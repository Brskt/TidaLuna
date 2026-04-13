pub(crate) mod session;

use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::connect::mdns::browser::{BrowserEvent, MdnsBrowser};
use crate::connect::types::{
    AuthInfo, MdnsDevice, MediaInfo, OAuthAuthInfo, OAuthFormParameters, OAuthParameters,
    OAuthServerInfo, PlaybackNotification, PlayerState, ServerInfo, SessionNotification,
};
use crate::connect::ws::client::{WsClient, WsClientEvent};

use session::{ControllerSession, ControllerSessionEvent};

/// Controller orchestrator: discovers devices, connects via WSS, manages session.
pub(crate) struct TidalConnectController {
    cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    browser: Option<MdnsBrowser>,
    session: Option<ControllerSession>,
    /// Currently discovered devices (cached for get_state snapshot)
    discovered_devices: Vec<MdnsDevice>,
    /// Last known player state from notifyPlayerStatusChanged
    last_player_state: PlayerState,
    /// Last known playback progress (ms)
    last_progress: u64,
    /// Last known media info from notifyMediaChanged
    last_media: Option<MediaInfo>,
    /// Server URLs and auth for building protocol messages
    content_server_url: String,
    queue_server_url: String,
    auth_server_url: String,
    auth_token: String,
    refresh_token: String,
    session_credential: Option<String>,
}

impl TidalConnectController {
    pub fn new(browser: Option<MdnsBrowser>) -> Self {
        Self {
            cancel: CancellationToken::new(),
            tasks: Vec::new(),
            browser,
            session: None,
            discovered_devices: Vec::new(),
            last_player_state: PlayerState::Idle,
            last_progress: 0,
            last_media: None,
            content_server_url: String::new(),
            queue_server_url: String::new(),
            auth_server_url: String::new(),
            auth_token: String::new(),
            refresh_token: String::new(),
            session_credential: None,
        }
    }

    /// Start mDNS discovery.
    pub fn start_discovery(&mut self) -> anyhow::Result<()> {
        if let Some(ref mut browser) = self.browser {
            browser.start_discovery()?;
        }
        Ok(())
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
    pub fn set_ws_and_start_session(
        &mut self,
        ws: Arc<WsClient>,
        device: &MdnsDevice,
        _credential: Option<&str>,
    ) {
        // Close old WS if switching device
        if let Some(ref mut old_session) = self.session {
            if old_session
                .connected_device()
                .is_some_and(|d| d.id == device.id)
            {
                // Same device reconnect - replace the dead WS, keep session state
                // so queue_start_or_resume sends resumeSession on the new socket.
                old_session.ws.shutdown();
                old_session.ws = ws;
                return;
            }
            // Different device - close old connection
            old_session.ws.shutdown();
        }
        let session = ControllerSession::new(ws);
        self.session = Some(session);
    }

    /// Start or resume the session (sync part - the WS send is fire-and-forget).
    pub fn start_session(&mut self, device: &MdnsDevice, credential: Option<&str>) {
        if let Some(ref mut session) = self.session {
            // Fire the start/resume command (async send, but send_raw is non-blocking via channel)
            let _ = session.queue_start_or_resume(device, credential);
        }
    }

    /// Disconnect from current device (sync - queues the endSession command).
    pub fn disconnect(&mut self, stop_casting: bool) {
        if let Some(ref mut session) = self.session {
            let _ = session.queue_end(stop_casting);
        }
        if stop_casting {
            self.session = None;
        }
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
                            self.last_media = Some(media_info);
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
                // Deduplicate by fullname
                if !self
                    .discovered_devices
                    .iter()
                    .any(|d| d.fullname == device.fullname)
                {
                    self.discovered_devices.push(device);
                }
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
    }

    pub fn discovered_devices(&self) -> &[MdnsDevice] {
        &self.discovered_devices
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

    pub fn set_server_urls(&mut self, queue_url: &str, content_url: &str, auth_url: &str) {
        self.queue_server_url = queue_url.trim_end_matches('/').to_string();
        self.content_server_url = content_url.trim_end_matches('/').to_string();
        self.auth_server_url = auth_url.trim_end_matches('/').to_string();
    }

    pub fn set_auth(&mut self, credential: &str, token: &str, refresh: &str) {
        self.session_credential = if credential.is_empty() {
            None
        } else {
            Some(credential.to_string())
        };
        self.auth_token = token.to_string();
        self.refresh_token = refresh.to_string();
    }

    pub fn session_credential(&self) -> Option<&str> {
        self.session_credential.as_deref()
    }

    /// Build a ServerInfo for the given type ("content" or "queue"), matching the JS serverInfo.js format.
    pub fn build_server_info(&self, server_type: &str) -> ServerInfo {
        let server_url = match server_type {
            "content" => &self.content_server_url,
            "queue" => &self.queue_server_url,
            _ => &self.content_server_url,
        };
        ServerInfo {
            server_url: server_url.clone(),
            auth_info: Some(AuthInfo {
                header_auth: None,
                oauth_server_info: Some(OAuthServerInfo {
                    server_url: self.auth_server_url.clone(),
                    auth_info: OAuthAuthInfo {
                        header_auth: Some(format!("Bearer {}", self.auth_token)),
                        oauth_parameters: Some(OAuthParameters {
                            access_token: self.auth_token.clone(),
                            refresh_token: self.refresh_token.clone(),
                        }),
                    },
                    form_parameters: Some(OAuthFormParameters {
                        grant_type: "switch_client".to_string(),
                        scope: "r_usr".to_string(),
                    }),
                    http_header_fields: Vec::new(),
                }),
                oauth_parameters: None,
            }),
            http_header_fields: Vec::new(),
            query_parameters: serde_json::Map::new(),
        }
    }

    /// Build content server info with audioquality in queryParameters.
    pub fn build_content_server_info(&self, quality: &str) -> ServerInfo {
        let mut info = self.build_server_info("content");
        info.query_parameters.insert(
            "audioquality".to_string(),
            serde_json::Value::String(quality.to_string()),
        );
        info.query_parameters.insert(
            "audiomode".to_string(),
            serde_json::Value::String("STEREO".to_string()),
        );
        info
    }

    pub fn shutdown(&mut self) {
        self.cancel.cancel();
        self.stop_discovery();
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
