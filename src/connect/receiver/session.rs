use std::collections::HashSet;
use tokio::sync::mpsc;

use crate::connect::types::{SessionStatus, consts};

// ── State machine ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReceiverSessionState {
    None,
    Active,
    Ended,
}

// ── Events emitted by the session manager ────────────────────────────

pub(crate) enum SessionNotifyEvent {
    Broadcast(serde_json::Value),
    Unicast {
        socket_id: u32,
        message: serde_json::Value,
    },
    Internal(SessionInternalEvent),
}

pub(crate) enum SessionInternalEvent {
    SessionStarted { session_id: String },
    SessionEnded,
    ResourcesGranted,
    ResourcesRevoked,
}

// ── Session data (mutable) ──────────────────────────────────────────

struct SessionData {
    session_id: String,
    _app_id: String,
    _app_name: String,
    _credential: Option<String>,
    /// Mutable status field - directly mutated, not via snapshot getter
    status: SessionStatus,
    connected_sockets: HashSet<u32>,
}

// ── Receiver-side session manager ────────────────────────────────────

pub(crate) struct ReceiverSession {
    state: ReceiverSessionState,
    session: Option<SessionData>,
    notify_tx: mpsc::Sender<SessionNotifyEvent>,
}

impl ReceiverSession {
    pub fn new(notify_tx: mpsc::Sender<SessionNotifyEvent>) -> Self {
        Self {
            state: ReceiverSessionState::None,
            session: None,
            notify_tx,
        }
    }

    /// Dispatch a session command from a mobile client.
    pub async fn handle_command(
        &mut self,
        socket_id: u32,
        command: &str,
        payload: &serde_json::Value,
    ) {
        match command {
            "startSession" => self.proc_start_session(socket_id, payload).await,
            "resumeSession" => self.proc_resume_session(socket_id, payload).await,
            "endSession" => self.proc_end_session(socket_id, payload).await,
            "releaseResources" => self.proc_release_resources(socket_id).await,
            "requestResources" => self.proc_request_resources(socket_id).await,
            _ => {
                crate::vprintln!("[connect::receiver::session] Unknown command: {}", command);
            }
        }
    }

    /// A WS client disconnected.
    pub async fn handle_disconnect(&mut self, socket_id: u32) {
        if let Some(ref mut session) = self.session {
            session.connected_sockets.remove(&socket_id);

            if session.connected_sockets.is_empty() {
                // Last client gone → suspend session (keep for resume)
                self.handle_session_ended(true).await;
            }
        }
    }

    // ── Command handlers ─────────────────────────────────────────────

    async fn proc_start_session(&mut self, socket_id: u32, payload: &serde_json::Value) {
        // If already active, end the old session first (suspended)
        if self.state == ReceiverSessionState::Active {
            self.handle_session_ended(true).await;
        }

        let app_id = payload
            .get("appId")
            .and_then(|v| v.as_str())
            .unwrap_or(consts::SESSION_APP_ID)
            .to_string();
        let app_name = payload
            .get("appName")
            .and_then(|v| v.as_str())
            .unwrap_or(consts::SESSION_APP_NAME)
            .to_string();
        let credential = payload
            .get("sessionCredential")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let session_id = generate_session_id();

        let mut sockets = HashSet::new();
        sockets.insert(socket_id);

        crate::vprintln!(
            "[connect::session] Start session: id={}, app={}:{}, cred={}",
            session_id,
            app_id,
            app_name,
            credential.is_some()
        );

        self.session = Some(SessionData {
            session_id: session_id.clone(),
            _app_id: app_id,
            _app_name: app_name,
            _credential: credential,
            status: SessionStatus::Active,
            connected_sockets: sockets,
        });
        self.state = ReceiverSessionState::Active;

        // Notify the client
        let notify = serde_json::json!({
            "command": "notifySessionStarted",
            "sessionId": session_id,
            "joined": false,
        });
        let _ = self
            .notify_tx
            .send(SessionNotifyEvent::Unicast {
                socket_id,
                message: notify,
            })
            .await;

        // Notify internal services
        let _ = self
            .notify_tx
            .send(SessionNotifyEvent::Internal(
                SessionInternalEvent::SessionStarted { session_id },
            ))
            .await;
    }

    async fn proc_resume_session(&mut self, socket_id: u32, payload: &serde_json::Value) {
        let requested_sid = payload
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        match self.state {
            ReceiverSessionState::Active => {
                if let Some(ref mut session) = self.session {
                    if session.session_id == requested_sid {
                        session.connected_sockets.insert(socket_id);
                        let notify = serde_json::json!({
                            "command": "notifySessionResumed",
                            "sessionId": session.session_id,
                        });
                        let _ = self
                            .notify_tx
                            .send(SessionNotifyEvent::Unicast {
                                socket_id,
                                message: notify,
                            })
                            .await;
                    } else {
                        self.send_session_error(socket_id, "resumeSession", "Invalid SessionId")
                            .await;
                    }
                }
            }
            ReceiverSessionState::Ended => {
                // Allow resume of suspended session
                if let Some(ref mut session) = self.session
                    && session.session_id == requested_sid
                    && session.status == SessionStatus::Suspended
                {
                    session.status = SessionStatus::Active;
                    session.connected_sockets.insert(socket_id);
                    self.state = ReceiverSessionState::Active;

                    let sid = session.session_id.clone();
                    let notify = serde_json::json!({
                        "command": "notifySessionResumed",
                        "sessionId": sid,
                    });
                    let _ = self
                        .notify_tx
                        .send(SessionNotifyEvent::Unicast {
                            socket_id,
                            message: notify,
                        })
                        .await;
                    let _ = self
                        .notify_tx
                        .send(SessionNotifyEvent::Internal(
                            SessionInternalEvent::SessionStarted { session_id: sid },
                        ))
                        .await;
                    return;
                }
                self.send_session_error(socket_id, "resumeSession", "Session ended")
                    .await;
            }
            ReceiverSessionState::None => {
                self.send_session_error(socket_id, "resumeSession", "No Session")
                    .await;
            }
        }
    }

    async fn proc_end_session(&mut self, socket_id: u32, payload: &serde_json::Value) {
        let stop_casting = payload
            .get("stopCasting")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if self.state != ReceiverSessionState::Active {
            self.send_session_error(socket_id, "endSession", "No active session")
                .await;
            return;
        }

        let session_id = self
            .session
            .as_ref()
            .map(|s| s.session_id.clone())
            .unwrap_or_default();

        // suspended = !stopCasting (if stopCasting=true → clean end, not suspended)
        self.handle_session_ended(!stop_casting).await;

        let notify = serde_json::json!({
            "command": "notifySessionEnded",
            "sessionId": session_id,
            "suspended": !stop_casting,
        });
        let _ = self
            .notify_tx
            .send(SessionNotifyEvent::Broadcast(notify))
            .await;
    }

    async fn proc_request_resources(&mut self, socket_id: u32) {
        // Grant resources immediately
        let msg = serde_json::json!({ "command": "grantResources" });
        let _ = self
            .notify_tx
            .send(SessionNotifyEvent::Unicast {
                socket_id,
                message: msg,
            })
            .await;
        let _ = self
            .notify_tx
            .send(SessionNotifyEvent::Internal(
                SessionInternalEvent::ResourcesGranted,
            ))
            .await;
    }

    async fn proc_release_resources(&mut self, _socket_id: u32) {
        let _ = self
            .notify_tx
            .send(SessionNotifyEvent::Internal(
                SessionInternalEvent::ResourcesRevoked,
            ))
            .await;
    }

    // ── Internal ─────────────────────────────────────────────────────

    /// Transition to Ended state.
    /// If suspended=true, keep the session for possible resume.
    /// Handle double-close correctly (idempotent).
    async fn handle_session_ended(&mut self, suspended: bool) {
        if let Some(ref mut session) = self.session {
            if session.status == SessionStatus::Suspended && !suspended {
                // Already suspended, now a real end → clear
                self.session = None;
            } else if suspended {
                session.status = SessionStatus::Suspended;
                // Session kept for resume
            } else {
                self.session = None;
            }
        }

        self.state = ReceiverSessionState::Ended;

        let _ = self
            .notify_tx
            .send(SessionNotifyEvent::Internal(
                SessionInternalEvent::SessionEnded,
            ))
            .await;
    }

    async fn send_session_error(&self, socket_id: u32, command: &str, error: &str) {
        let msg = serde_json::json!({
            "command": "notifySessionError",
            "requestedCommand": command,
            "error": error,
        });
        let _ = self
            .notify_tx
            .send(SessionNotifyEvent::Unicast {
                socket_id,
                message: msg,
            })
            .await;
    }
}

/// Generate a UUID v4 session ID.
fn generate_session_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("RNG failed");
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u16::from_be_bytes([bytes[4], bytes[5]]),
        (u16::from_be_bytes([bytes[6], bytes[7]]) & 0x0fff) | 0x4000,
        (u16::from_be_bytes([bytes[8], bytes[9]]) & 0x3fff) | 0x8000,
        u64::from_be_bytes([
            0, 0, bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
        ]),
    )
}
