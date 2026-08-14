//! HTTP / OAuth layer for the queue subsystem.
//!
//! Functions here are stateless: they take references to the `reqwest`
//! client and to the relevant `ServerInfo` / `OAuthServerInfo` and return a
//! `Result`. They never touch `QueueState`. The façade (`QueueManager`)
//! owns all state mutation; on a successful refresh it performs the
//! `AuthStore` CAS itself and then invokes `update_access_token` to keep
//! the wire-shaped `ServerInfo` in sync.

use crate::connect::consts;
use crate::connect::types::{OAuthServerInfo, ServerInfo};

use super::QueueError;

/// Build an `Authorization` header value from a `ServerInfo`. Prefers an
/// already-formatted `header_auth`; otherwise falls back to
/// `"Bearer {oauth_parameters.access_token}"`. Handles both the top-level
/// `auth_info` and the nested `oauth_server_info.auth_info`.
pub(super) fn resolve_auth_header(server: &ServerInfo) -> String {
    if let Some(ref ai) = server.auth_info {
        if let Some(ref ha) = ai.header_auth {
            return ha.clone();
        }
        if let Some(ref params) = ai.oauth_parameters {
            return format!("Bearer {}", params.access_token);
        }
        if let Some(ref oauth_server) = ai.oauth_server_info {
            if let Some(ref ha) = oauth_server.auth_info.header_auth {
                return ha.clone();
            }
            if let Some(ref params) = oauth_server.auth_info.oauth_parameters {
                return format!("Bearer {}", params.access_token);
            }
        }
    }
    String::new()
}

/// Read `(access_token, refresh_token, scope)` from the OAuth parameters
/// buried inside a `ServerInfo`. Returns `None` if the ServerInfo does not
/// carry OAuth credentials. Prefers the outer `auth_info.oauth_parameters`
/// and falls back to the nested `oauth_server_info.auth_info.oauth_parameters`.
pub(super) fn extract_oauth_params(
    server: Option<&ServerInfo>,
) -> Option<(String, String, Option<String>)> {
    let ai = server.and_then(|s| s.auth_info.as_ref())?;
    let scope = ai
        .oauth_server_info
        .as_ref()
        .and_then(|o| o.form_parameters.as_ref())
        .map(|f| f.scope.clone());

    if let Some(p) = ai.oauth_parameters.as_ref() {
        return Some((p.access_token.clone(), p.refresh_token.clone(), scope));
    }
    if let Some(p) = ai
        .oauth_server_info
        .as_ref()
        .and_then(|o| o.auth_info.oauth_parameters.as_ref())
    {
        return Some((p.access_token.clone(), p.refresh_token.clone(), scope));
    }
    None
}

/// Parse the OAuth `error` field from a refresh-response body. Returns the
/// error code (e.g. `"invalid_grant"`, `"invalid_request"`) or `None` if
/// the body is not valid JSON or does not carry an `error` field.
fn parse_oauth_error_code(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value
        .get("error")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// True if `url` is an https URL on a TIDAL-owned API host (`*.tidal.com` or
/// `*.tidalhifi.com`). Connect server URLs are peer-supplied; the receiver
/// must never attach its token to a host it does not control.
pub(super) fn is_trusted_server_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    let host = parsed.host_str().unwrap_or("");
    host == "tidal.com"
        || host.ends_with(".tidal.com")
        || host == "tidalhifi.com"
        || host.ends_with(".tidalhifi.com")
}

/// GET a JSON resource on the queue server, using the server's auth header.
pub(super) async fn get_with_auth(
    http: &reqwest::Client,
    queue_server: Option<&ServerInfo>,
    url: &str,
) -> Result<serde_json::Value, QueueError> {
    if !is_trusted_server_url(url) {
        return Err(QueueError::UntrustedServer);
    }
    let server = queue_server.ok_or(QueueError::NoServer)?;
    let auth_header = resolve_auth_header(server);

    let response = http
        .get(url)
        .header("Authorization", &auth_header)
        .timeout(std::time::Duration::from_secs(consts::HTTP_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| QueueError::Network(e.to_string()))?;

    let status = response.status();
    if status.as_u16() == 401 {
        return Err(QueueError::TokenExpired);
    }
    if !status.is_success() {
        return Err(QueueError::HttpStatus(status.as_u16()));
    }

    response
        .json::<serde_json::Value>()
        .await
        .map_err(|e| QueueError::InvalidResponse(e.to_string()))
}

/// POST a JSON body to the queue server. The response body is discarded;
/// only HTTP success is reported.
pub(super) async fn post_with_auth(
    http: &reqwest::Client,
    queue_server: Option<&ServerInfo>,
    url: &str,
    body: &serde_json::Value,
) -> Result<(), QueueError> {
    if !is_trusted_server_url(url) {
        return Err(QueueError::UntrustedServer);
    }
    let server = queue_server.ok_or(QueueError::NoServer)?;
    let auth_header = resolve_auth_header(server);

    http.post(url)
        .header("Authorization", &auth_header)
        .json(body)
        .timeout(std::time::Duration::from_secs(consts::HTTP_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| QueueError::Network(e.to_string()))?;
    Ok(())
}

/// Successful OAuth refresh outcome. The server may rotate the refresh
/// token (OAuth 2.0 §6): a new one is reported when present.
pub(super) struct RefreshSuccess {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

/// POST an OAuth `grant_type=refresh_token` request. Stateless: the caller
/// is responsible for taking the current `refresh_token` from the
/// `AuthStore` snapshot and for installing the new credentials via a CAS
/// afterwards.
///
/// Classifies server responses:
/// * 2xx with `access_token` -> `Ok(RefreshSuccess)`
/// * 4xx/5xx carrying `error: "invalid_grant"` -> `Err(AuthTerminated)`
/// * Other non-success statuses -> `Err(HttpStatus)`
/// * Network / transport errors -> `Err(Network)`
/// * Parse errors -> `Err(InvalidResponse)`
pub(super) async fn refresh_token(
    http: &reqwest::Client,
    oauth: &OAuthServerInfo,
    current_refresh_token: &str,
) -> Result<RefreshSuccess, QueueError> {
    if !is_trusted_server_url(&oauth.server_url) {
        return Err(QueueError::UntrustedServer);
    }
    let auth_header = oauth
        .auth_info
        .header_auth
        .as_deref()
        .unwrap_or("")
        .to_string();

    let mut form = std::collections::HashMap::new();
    if let Some(ref fp) = oauth.form_parameters {
        form.insert("grant_type".to_string(), fp.grant_type.clone());
        form.insert("scope".to_string(), fp.scope.clone());
    }
    form.insert(
        "refresh_token".to_string(),
        current_refresh_token.to_string(),
    );

    let resp = http
        .post(&oauth.server_url)
        .header("Authorization", &auth_header)
        .form(&form)
        .timeout(std::time::Duration::from_secs(consts::HTTP_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| QueueError::Network(e.to_string()))?;

    let status = resp.status();
    let body_bytes = resp
        .bytes()
        .await
        .map_err(|e| QueueError::Network(e.to_string()))?;

    if !status.is_success() {
        if let Some(err_code) = parse_oauth_error_code(&body_bytes)
            && err_code == "invalid_grant"
        {
            return Err(QueueError::AuthTerminated {
                provider_error: err_code,
            });
        }
        return Err(QueueError::HttpStatus(status.as_u16()));
    }

    let body: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| QueueError::InvalidResponse(e.to_string()))?;

    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or(QueueError::MissingField("access_token"))?
        .to_string();

    let rotated_refresh = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok(RefreshSuccess {
        access_token,
        refresh_token: rotated_refresh,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "../../../../tests/unit/connect/receiver/queue/http.rs"]
mod tests;
