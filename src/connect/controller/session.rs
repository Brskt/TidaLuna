use std::sync::Arc;

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
    ConnectionLost,
}

/// A session to move onto a new socket, live or kept after a loss or a suspension, as it stood when
/// the connect asked for it: the install only goes ahead if the session is still this one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResumePlan {
    pub(crate) session_id: String,
    connection: u64,
}

/// Controller-side session manager: tracks the session the device confirmed (started or resumed
/// by the connect that dialled it) and sends it endSession.
pub(crate) struct ControllerSession {
    current: Option<ActiveSession>,
    pub(crate) ws: Arc<WsClient>,
    /// The controller's `connection_gen` when `ws` was installed.
    pub(super) connection: u64,
}

impl ControllerSession {
    /// The session the device started on `ws`, live, with the start TIDAL is owed.
    pub(super) fn started(
        ws: Arc<WsClient>,
        connection: u64,
        device: MdnsDevice,
        session_id: String,
        joined: bool,
    ) -> (Self, ControllerSessionEvent) {
        let event = ControllerSessionEvent::SessionStarted {
            session_id: session_id.clone(),
            device: device.clone(),
            joined,
            connection,
        };
        let session = Self {
            current: Some(ActiveSession {
                session_id,
                device,
                joined,
                live: true,
            }),
            ws,
            connection,
        };
        (session, event)
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
            // Settled by the connect that sent the startSession, which announced it.
            SessionNotification::NotifySessionStarted { .. } => None,
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
            // Settled by the connect that sent the resumeSession, which announced it.
            SessionNotification::NotifySessionResumed { .. } => None,
            SessionNotification::NotifySessionError {
                requested_command,
                details,
            } => Some(ControllerSessionEvent::SessionError {
                command: requested_command.clone(),
                details: details.clone(),
            }),
            SessionNotification::NotifySessionState { .. } => None,
        }
    }

    /// The device resumed the session on the current socket: the start TIDAL is owed, unless it
    /// holds the session live already. TIDAL's DEVICE_CONNECTED comes from a start alone.
    pub(super) fn resumed(&mut self) -> Option<ControllerSessionEvent> {
        let session = self.current.as_mut()?;
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

    /// Handle connection lost (ping timeout or WS close).
    pub fn handle_connection_lost(&mut self) -> Option<ControllerSessionEvent> {
        // Not cleared (the session may be resumable), though TIDAL reads the loss as a disconnect.
        if let Some(current) = self.current.as_mut() {
            current.live = false;
        }
        Some(ControllerSessionEvent::ConnectionLost)
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

    /// The plan for moving this session to a new socket of the device `fullname`: a session of
    /// that device has one, live or kept after a loss or a suspension.
    pub(super) fn resume_plan(&self, fullname: &str) -> Option<ResumePlan> {
        let current = self.current.as_ref()?;
        (current.device.fullname == fullname).then(|| ResumePlan {
            session_id: current.session_id.clone(),
            connection: self.connection,
        })
    }

    /// Whether this is still the session `plan` was made for, on `device`.
    pub(super) fn is_planned(&self, plan: &ResumePlan, device: &MdnsDevice) -> bool {
        self.connection == plan.connection
            && self.current.as_ref().is_some_and(|s| {
                s.session_id == plan.session_id && s.device.fullname == device.fullname
            })
    }

    /// The live session only: a page reload replays it as DEVICE_CONNECTED.
    pub fn session_info(&self) -> Option<(&str, &MdnsDevice, bool, u64)> {
        self.current
            .as_ref()
            .filter(|_| self.is_live())
            .map(|s| (s.session_id.as_str(), &s.device, s.joined, self.connection))
    }
}
