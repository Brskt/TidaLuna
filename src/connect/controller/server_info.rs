//! The ServerInfo a device is told to use: TIDAL's server URLs and the token to present to them.
//!
//! Neither comes from the renderer. It hosts plugins, which can call `initialize` and
//! `setAuthentication` themselves, and Rust already holds TIDAL's hosts and the real token.

use crate::connect::types::{
    AuthInfo, OAuthAuthInfo, OAuthFormParameters, OAuthParameters, OAuthServerInfo, ServerInfo,
};
use crate::connect::ws::client::PendingReply;
use crate::ui::nav::{HOST_API, HOST_AUTH, PATH_OAUTH_TOKEN};

const HOST_QUEUE: &str = "connectqueue.tidal.com";
const PATH_QUEUE: &str = "/v1/queues";
const PATH_CONTENT: &str = "/v1";

fn https(host: &str, path: &str) -> String {
    format!("https://{host}{path}")
}

/// A ServerInfo for `server_type` ("content" or "queue"), matching the JS serverInfo.js format.
/// The refresh token stays empty: TIDAL's own `setAuthentication` never passes one either.
pub(crate) fn build_server_info(server_type: &str, access_token: &str) -> ServerInfo {
    let server_url = match server_type {
        "queue" => https(HOST_QUEUE, PATH_QUEUE),
        _ => https(HOST_API, PATH_CONTENT),
    };
    ServerInfo {
        server_url,
        auth_info: Some(AuthInfo {
            header_auth: None,
            oauth_server_info: Some(OAuthServerInfo {
                server_url: https(HOST_AUTH, PATH_OAUTH_TOKEN),
                auth_info: OAuthAuthInfo {
                    header_auth: Some(format!("Bearer {access_token}")),
                    oauth_parameters: Some(OAuthParameters {
                        access_token: access_token.to_string(),
                        refresh_token: String::new(),
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

/// What a device was handed as server info on one connection: the token and the quality it carried,
/// stamped with the order it was recorded in.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Delivery {
    seq: u64,
    conn_gen: u64,
    token: String,
    quality: Option<String>,
}

impl Delivery {
    #[cfg(test)]
    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }

    /// Whether `other` hands the device what this one does, whichever delivery it is.
    fn carries_same_as(&self, other: &Delivery) -> bool {
        (self.conn_gen, &self.token, &self.quality)
            == (other.conn_gen, &other.token, &other.quality)
    }
}

/// A delivery whose frame is on the socket's write queue, with the answer still to come.
pub(crate) struct Queued {
    pub(crate) delivery: Delivery,
    pub(crate) reply: PendingReply,
}

/// What became of a device's server info being owed.
pub(crate) enum ServerInfoClaim {
    NoSession,
    Held,
    /// Recorded as handed over and queued: `ServerInfoLedger::release` takes it back when the
    /// answer does not come.
    Queued(Queued),
    /// Recorded and taken back: the frame could not be queued.
    Failed(String),
}

/// The last `updateServerInfo` delivery to the session's device. TIDAL sends one per
/// `setAuthentication`; a repeat is not sent here, to keep a looping one from flooding the device. A
/// `loadCloudQueue` is no delivery: it does not lift "token expired".
#[derive(Default)]
pub(crate) struct ServerInfoLedger {
    sent: Option<Delivery>,
    next_seq: u64,
}

impl ServerInfoLedger {
    /// Records `token` and `quality` as handed to the device of connection `conn_gen`. `None`
    /// when the device holds them already.
    pub(crate) fn claim(
        &mut self,
        conn_gen: u64,
        token: &str,
        quality: Option<&str>,
    ) -> Option<Delivery> {
        let delivery = Delivery {
            seq: self.next_seq + 1,
            conn_gen,
            token: token.to_owned(),
            quality: quality.map(str::to_owned),
        };
        if self
            .sent
            .as_ref()
            .is_some_and(|sent| sent.carries_same_as(&delivery))
        {
            return None;
        }
        self.next_seq = delivery.seq;
        self.sent = Some(delivery.clone());
        Some(delivery)
    }

    /// A send that failed handed nothing over, unless a later delivery has replaced the record.
    pub(crate) fn release(&mut self, delivery: &Delivery) {
        if self.sent.as_ref() == Some(delivery) {
            self.sent = None;
        }
    }

    /// The device reported its server info failing: what it holds is no longer worth keeping a
    /// record of, and the next delivery is owed even for the same token.
    pub(crate) fn forget_sent(&mut self) {
        self.sent = None;
    }
}

/// Content server info with audioquality in queryParameters.
pub(crate) fn build_content_server_info(quality: &str, access_token: &str) -> ServerInfo {
    let mut info = build_server_info("content", access_token);
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

#[cfg(test)]
#[path = "../../../tests/unit/connect/controller/server_info.rs"]
mod tests;
