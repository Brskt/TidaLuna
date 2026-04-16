//! Protocol types shared across the connect module.
//!
//! Split by domain to keep each file reviewable. Imports from outside
//! `connect::types` continue to work via the flat re-exports below.

pub(crate) mod auth;
pub(crate) mod device;
pub(crate) mod media;
pub(crate) mod playback;
pub(crate) mod queue;
pub(crate) mod session;

pub(crate) use auth::{
    AuthInfo, OAuthAuthInfo, OAuthFormParameters, OAuthParameters, OAuthServerInfo, ServerInfo,
};
pub(crate) use device::{DeviceType, MdnsDevice};
pub(crate) use media::{MediaInfo, MediaMetadata, default_policy};
pub(crate) use playback::{PbPlayState, PbState, PlaybackNotification, PlayerState};
#[allow(unused_imports)]
pub(crate) use queue::QueueItemDisplayInfo;
pub(crate) use queue::{QueueChangeReason, QueueInfo, QueueItem, QueueNotification, RepeatMode};
pub(crate) use session::{SessionCommand, SessionNotification, SessionStatus};

// ── Receiver config ──────────────────────────────────────────────────

pub(crate) struct ReceiverConfig {
    pub ws_port: u16,
    pub friendly_name: String,
    pub model_name: String,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            ws_port: crate::connect::consts::WS_DEFAULT_PORT,
            friendly_name: format!(
                "TidaLunar: {}",
                gethostname::gethostname().to_string_lossy()
            ),
            model_name: "TidaLunar".to_string(),
        }
    }
}
