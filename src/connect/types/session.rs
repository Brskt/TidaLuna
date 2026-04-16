use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionStatus {
    Active,
    Suspended,
}

// Session commands (controller → device)
#[derive(Serialize, Debug)]
#[serde(tag = "command", rename_all = "camelCase")]
#[allow(clippy::enum_variant_names)]
pub(crate) enum SessionCommand {
    #[serde(rename = "startSession")]
    StartSession {
        #[serde(rename = "appId")]
        app_id: String,
        #[serde(rename = "appName")]
        app_name: String,
        #[serde(rename = "sessionCredential", skip_serializing_if = "Option::is_none")]
        session_credential: Option<String>,
    },
    #[serde(rename = "resumeSession")]
    ResumeSession {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    #[serde(rename = "endSession")]
    EndSession {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "stopCasting")]
        stop_casting: bool,
    },
}

// Session notifications (device → controller)
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "command", rename_all = "camelCase")]
#[allow(clippy::enum_variant_names)]
pub(crate) enum SessionNotification {
    #[serde(rename = "notifySessionStarted")]
    NotifySessionStarted {
        #[serde(rename = "sessionId")]
        session_id: String,
        joined: bool,
    },
    #[serde(rename = "notifySessionResumed")]
    NotifySessionResumed {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    #[serde(rename = "notifySessionEnded")]
    NotifySessionEnded {
        #[serde(rename = "sessionId")]
        session_id: String,
        suspended: bool,
    },
    #[serde(rename = "notifySessionError")]
    NotifySessionError {
        #[serde(rename = "requestedCommand")]
        requested_command: String,
        #[serde(flatten)]
        details: serde_json::Value,
    },
    #[serde(rename = "notifySessionState")]
    NotifySessionState { state: serde_json::Value },
}
