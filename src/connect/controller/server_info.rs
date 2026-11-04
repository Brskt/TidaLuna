//! The ServerInfo a device is told to use: TIDAL's server URLs and the token to present to them.
//!
//! Neither comes from the renderer. It hosts plugins, which can call `initialize` and
//! `setAuthentication` themselves, and Rust already holds TIDAL's hosts and the real token.

use crate::connect::types::{
    AuthInfo, OAuthAuthInfo, OAuthFormParameters, OAuthParameters, OAuthServerInfo, ServerInfo,
};
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
