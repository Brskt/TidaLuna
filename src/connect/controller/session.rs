use std::sync::Arc;

use crate::connect::consts;
use crate::connect::types::{MdnsDevice, SessionCommand, SessionNotification};
use crate::connect::ws::client::WsClient;

struct ActiveSession {
    session_id: String,
    device: MdnsDevice,
    joined: bool,
}

pub(crate) enum ControllerSessionEvent {
    SessionStarted {
        session_id: String,
        device: MdnsDevice,
        joined: bool,
    },
    SessionEnded {
        suspended: bool,
    },
    SessionError {
        command: String,
        details: serde_json::Value,
    },
    ConnectionLost,
}

/// Controller-side session manager.
/// Sends startSession/resumeSession/endSession to the remote device,
/// tracks the active session, and correlates notifySessionStarted via nonce.
pub(crate) struct ControllerSession {
    current: Option<ActiveSession>,
    pub(crate) ws: Arc<WsClient>,
    /// Internal nonce to correlate startSession → notifySessionStarted
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
    ) -> anyhow::Result<()> {
        // Same device + session exists → resume
        if let Some(ref session) = self.current
            && session.device.id == device.id
        {
            let cmd = SessionCommand::ResumeSession {
                session_id: session.session_id.clone(),
            };
            self.fire_and_forget(serde_json::to_value(&cmd)?);
            return Ok(());
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
        Ok(())
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

                let device = self
                    .pending_device
                    .take()
                    .expect("pending_device must be set when pending_start_nonce is set");

                self.current = Some(ActiveSession {
                    session_id: session_id.clone(),
                    device: device.clone(),
                    joined: *joined,
                });

                Some(ControllerSessionEvent::SessionStarted {
                    session_id: session_id.clone(),
                    device,
                    joined: *joined,
                })
            }
            SessionNotification::NotifySessionEnded {
                session_id,
                suspended,
            } => {
                if self
                    .current
                    .as_ref()
                    .is_some_and(|s| s.session_id == *session_id)
                    && !suspended
                {
                    self.current = None;
                }
                Some(ControllerSessionEvent::SessionEnded {
                    suspended: *suspended,
                })
            }
            SessionNotification::NotifySessionResumed { session_id } => {
                // Update joined flag if needed
                if let Some(ref mut session) = self.current
                    && session.session_id == *session_id
                {
                    session.joined = true;
                }
                None
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
        self.pending_start_nonce = None;
        self.pending_device = None;
        // Don't clear the session - it may be resumable
        Some(ControllerSessionEvent::ConnectionLost)
    }

    pub fn connected_device(&self) -> Option<&MdnsDevice> {
        self.current.as_ref().map(|s| &s.device)
    }

    pub fn session_info(&self) -> Option<(&str, &MdnsDevice, bool)> {
        self.current
            .as_ref()
            .map(|s| (s.session_id.as_str(), &s.device, s.joined))
    }
}
