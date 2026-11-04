use std::sync::Arc;

use crate::connect::consts;
use crate::connect::types::{MdnsDevice, SessionCommand, SessionNotification};
use crate::connect::ws::client::WsClient;

struct ActiveSession {
    session_id: String,
    device: MdnsDevice,
    joined: bool,
    /// Whether TIDAL was last told this session is connected: set where `SessionStarted` is
    /// returned, cleared where an end or a lost connection is. A session kept to resume is not live.
    live: bool,
}

pub(crate) enum ControllerSessionEvent {
    SessionStarted {
        session_id: String,
        device: MdnsDevice,
        joined: bool,
        /// The socket generation this start rides on: a session resumed on a new socket carries
        /// its old id; the id alone cannot tell a repeat announcement from a resume.
        connection: u64,
    },
    SessionEnded {
        suspended: bool,
    },
    SessionError {
        command: String,
        details: serde_json::Value,
    },
    /// `during_start`: the socket died before the device answered our startSession.
    ConnectionLost {
        during_start: bool,
    },
}

impl ControllerSessionEvent {
    /// The device refused the startSession or resumeSession a connect sent: that connect failed
    /// after the dial, and the device holds no session for it.
    pub(crate) fn is_refused_connect(&self) -> bool {
        matches!(self, Self::SessionError { command, .. }
            if command == "startSession" || command == "resumeSession")
    }
}

/// What `queue_start_or_resume` sent to the device.
pub(crate) enum SessionRequest {
    Start,
    Resume,
}

impl SessionRequest {
    /// The `joined` TIDAL reads off a `connectToDevice` answer: the device already held our session.
    pub(crate) fn joined(&self) -> bool {
        matches!(self, Self::Resume)
    }
}

/// Controller-side session manager.
/// Sends startSession/resumeSession/endSession to the remote device,
/// tracks the active session, and correlates notifySessionStarted via nonce.
pub(crate) struct ControllerSession {
    current: Option<ActiveSession>,
    pub(crate) ws: Arc<WsClient>,
    /// The controller's `connection_gen` when `ws` was installed.
    pub(super) connection: u64,
    /// Internal nonce to correlate startSession -> notifySessionStarted
    /// (startSession has no requestId - nonce correlates the response)
    pending_start_nonce: Option<u64>,
    nonce_counter: u64,
    /// Device being connected to (stored during start_or_resume, consumed on notification)
    pending_device: Option<MdnsDevice>,
}

impl ControllerSession {
    pub fn new(ws: Arc<WsClient>) -> Self {
        Self {
            current: None,
            ws,
            connection: 0,
            pending_start_nonce: None,
            nonce_counter: 0,
            pending_device: None,
        }
    }

    /// Queue a start or resume session command (non-blocking).
    pub fn queue_start_or_resume(
        &mut self,
        device: &MdnsDevice,
        credential: Option<&str>,
    ) -> anyhow::Result<SessionRequest> {
        // Same device + session exists -> resume
        if let Some(ref session) = self.current
            && session.device.id == device.id
        {
            let cmd = SessionCommand::ResumeSession {
                session_id: session.session_id.clone(),
            };
            self.fire_and_forget(serde_json::to_value(&cmd)?);
            return Ok(SessionRequest::Resume);
        }

        // New session
        self.nonce_counter += 1;
        self.pending_start_nonce = Some(self.nonce_counter);
        self.pending_device = Some(device.clone());

        let cmd = SessionCommand::StartSession {
            app_id: consts::SESSION_APP_ID.to_string(),
            app_name: consts::SESSION_APP_NAME.to_string(),
            session_credential: credential.map(|s| s.to_string()),
        };
        self.fire_and_forget(serde_json::to_value(&cmd)?);
        Ok(SessionRequest::Start)
    }

    /// Queue an end session command (non-blocking).
    pub fn queue_end(&mut self, stop_casting: bool) -> anyhow::Result<()> {
        let session_id = match &self.current {
            Some(s) => s.session_id.clone(),
            None => anyhow::bail!("No active session"),
        };

        let cmd = SessionCommand::EndSession {
            session_id,
            stop_casting,
        };
        self.fire_and_forget(serde_json::to_value(&cmd)?);
        Ok(())
    }

    /// Send a command via WS without waiting (spawn async send).
    fn fire_and_forget(&self, value: serde_json::Value) {
        let ws = self.ws.clone();
        if let Some(rt) = crate::state::RT_HANDLE.get() {
            rt.spawn(async move {
                let _ = ws.send_raw(value).await;
            });
        }
    }

    /// Handle a session notification from the device.
    /// Returns an event to propagate to the frontend, if applicable.
    pub fn handle_notification(
        &mut self,
        notification: &SessionNotification,
    ) -> Option<ControllerSessionEvent> {
        match notification {
            SessionNotification::NotifySessionStarted { session_id, joined } => {
                // Only accept if we have a pending start (nonce correlation)
                if self.pending_start_nonce.take().is_none() {
                    crate::vprintln!(
                        "[connect::controller::session] Ignoring stale notifySessionStarted"
                    );
                    return None;
                }

                let Some(device) = self.pending_device.take() else {
                    crate::vprintln!(
                        "[connect::controller::session] notifySessionStarted without a pending device"
                    );
                    return None;
                };

                self.current = Some(ActiveSession {
                    session_id: session_id.clone(),
                    device: device.clone(),
                    joined: *joined,
                    live: true,
                });

                Some(ControllerSessionEvent::SessionStarted {
                    session_id: session_id.clone(),
                    device,
                    joined: *joined,
                    connection: self.connection,
                })
            }
            SessionNotification::NotifySessionEnded {
                session_id,
                suspended,
            } => {
                // TIDAL reads any end as a disconnect (its reducer checks no id): the
                // session stops being live whichever id the device named.
                if let Some(current) = self.current.as_mut() {
                    current.live = false;
                }
                if !*suspended
                    && self
                        .current
                        .as_ref()
                        .is_some_and(|s| s.session_id == *session_id)
                {
                    self.current = None;
                }
                Some(ControllerSessionEvent::SessionEnded {
                    suspended: *suspended,
                })
            }
            SessionNotification::NotifySessionResumed { session_id } => {
                // TIDAL's `isConnected` only turns true on DEVICE_CONNECTED, which the web app
                // dispatches for TIDAL Connect from `TCnotifySessionStarted` alone: a resume has to
                // arrive as that, with `resumed: true`.
                let session = self
                    .current
                    .as_mut()
                    .filter(|s| s.session_id == *session_id)?;
                session.joined = true;
                if session.live {
                    return None;
                }
                session.live = true;
                Some(ControllerSessionEvent::SessionStarted {
                    session_id: session.session_id.clone(),
                    device: session.device.clone(),
                    joined: true,
                    connection: self.connection,
                })
            }
            SessionNotification::NotifySessionError {
                requested_command,
                details,
            } => {
                // Clear pending start if the error is for startSession
                if requested_command == "startSession" {
                    self.pending_start_nonce = None;
                    self.pending_device = None;
                }
                Some(ControllerSessionEvent::SessionError {
                    command: requested_command.clone(),
                    details: details.clone(),
                })
            }
            SessionNotification::NotifySessionState { .. } => None,
        }
    }

    /// Handle connection lost (ping timeout or WS close).
    pub fn handle_connection_lost(&mut self) -> Option<ControllerSessionEvent> {
        let during_start = self.pending_start_nonce.is_some();
        self.pending_start_nonce = None;
        self.pending_device = None;
        // Not cleared (the session may be resumable), though TIDAL reads the loss as a disconnect.
        if let Some(current) = self.current.as_mut() {
            current.live = false;
        }
        Some(ControllerSessionEvent::ConnectionLost { during_start })
    }

    /// What TIDAL's `isConnected` says: a session it was told started or resumed and has not been
    /// told ended or lost since. A socket that died with its `ConnectionLost` still on the way
    /// already reads as gone.
    pub fn is_live(&self) -> bool {
        self.current.as_ref().is_some_and(|s| s.live) && self.ws.is_connected()
    }

    /// Takes the session out of the live state. Whether it was live: when it was, TIDAL has to be
    /// told it ended, as nothing it sees would say so.
    pub(super) fn end_live(&mut self) -> bool {
        self.current
            .as_mut()
            .is_some_and(|s| std::mem::replace(&mut s.live, false))
    }

    pub fn connected_device(&self) -> Option<&MdnsDevice> {
        self.current.as_ref().map(|s| &s.device)
    }

    /// The live session only: a page reload replays it as DEVICE_CONNECTED.
    pub fn session_info(&self) -> Option<(&str, &MdnsDevice, bool, u64)> {
        self.current
            .as_ref()
            .filter(|_| self.is_live())
            .map(|s| (s.session_id.as_str(), &s.device, s.joined, self.connection))
    }
}
