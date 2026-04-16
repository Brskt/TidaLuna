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

/// GET a JSON resource on the queue server, using the server's auth header.
pub(super) async fn get_with_auth(
    http: &reqwest::Client,
    queue_server: Option<&ServerInfo>,
    url: &str,
) -> Result<serde_json::Value, QueueError> {
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
/// token (OAuth 2.0 §6), so a new one is reported when present.
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
/// * 2xx with `access_token` → `Ok(RefreshSuccess)`
/// * 4xx/5xx carrying `error: "invalid_grant"` → `Err(AuthTerminated)`
/// * Other non-success statuses → `Err(HttpStatus)`
/// * Network / transport errors → `Err(Network)`
/// * Parse errors → `Err(InvalidResponse)`
pub(super) async fn refresh_token(
    http: &reqwest::Client,
    oauth: &OAuthServerInfo,
    current_refresh_token: &str,
) -> Result<RefreshSuccess, QueueError> {
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
mod tests {
    use super::*;
    use crate::connect::types::{
        AuthInfo, OAuthAuthInfo, OAuthFormParameters, OAuthParameters, OAuthServerInfo, ServerInfo,
    };

    fn empty_server(auth_info: Option<AuthInfo>) -> ServerInfo {
        ServerInfo {
            server_url: "https://api.tidal.com".into(),
            auth_info,
            http_header_fields: vec![],
            query_parameters: serde_json::Map::new(),
        }
    }

    fn oauth_params(access: &str, refresh: &str) -> OAuthParameters {
        OAuthParameters {
            access_token: access.into(),
            refresh_token: refresh.into(),
        }
    }

    // ── resolve_auth_header ──────────────────────────────────────────

    #[test]
    fn resolve_auth_header_prefers_header_auth_literal() {
        // When `header_auth` is populated it is returned verbatim, even if
        // outer oauth_parameters also carries a token: the literal header
        // may include schemes or signatures that cannot be reconstructed
        // from the plain access_token alone.
        let server = empty_server(Some(AuthInfo {
            header_auth: Some("Bearer from-header".into()),
            oauth_server_info: None,
            oauth_parameters: Some(oauth_params("at-outer", "rt-outer")),
        }));
        assert_eq!(resolve_auth_header(&server), "Bearer from-header");
    }

    #[test]
    fn resolve_auth_header_falls_back_to_outer_oauth_parameters() {
        let server = empty_server(Some(AuthInfo {
            header_auth: None,
            oauth_server_info: None,
            oauth_parameters: Some(oauth_params("at-outer", "rt-outer")),
        }));
        assert_eq!(resolve_auth_header(&server), "Bearer at-outer");
    }

    #[test]
    fn resolve_auth_header_falls_back_to_nested_oauth_parameters() {
        let server = empty_server(Some(AuthInfo {
            header_auth: None,
            oauth_server_info: Some(OAuthServerInfo {
                server_url: "https://auth.tidal.com".into(),
                auth_info: OAuthAuthInfo {
                    header_auth: None,
                    oauth_parameters: Some(oauth_params("at-nested", "rt-nested")),
                },
                form_parameters: None,
                http_header_fields: vec![],
            }),
            oauth_parameters: None,
        }));
        assert_eq!(resolve_auth_header(&server), "Bearer at-nested");
    }

    #[test]
    fn resolve_auth_header_empty_when_no_credentials() {
        let server = empty_server(None);
        assert_eq!(resolve_auth_header(&server), "");
    }

    // ── extract_oauth_params ─────────────────────────────────────────

    #[test]
    fn extract_oauth_params_prefers_outer() {
        let server = empty_server(Some(AuthInfo {
            header_auth: None,
            oauth_server_info: Some(OAuthServerInfo {
                server_url: "https://auth".into(),
                auth_info: OAuthAuthInfo {
                    header_auth: None,
                    oauth_parameters: Some(oauth_params("at-nested", "rt-nested")),
                },
                form_parameters: Some(OAuthFormParameters {
                    grant_type: "refresh_token".into(),
                    scope: "r_usr".into(),
                }),
                http_header_fields: vec![],
            }),
            oauth_parameters: Some(oauth_params("at-outer", "rt-outer")),
        }));
        let (access, refresh, scope) = extract_oauth_params(Some(&server)).unwrap();
        assert_eq!(access, "at-outer");
        assert_eq!(refresh, "rt-outer");
        assert_eq!(scope.as_deref(), Some("r_usr"));
    }

    #[test]
    fn extract_oauth_params_falls_back_to_nested_when_outer_missing() {
        let server = empty_server(Some(AuthInfo {
            header_auth: None,
            oauth_server_info: Some(OAuthServerInfo {
                server_url: "https://auth".into(),
                auth_info: OAuthAuthInfo {
                    header_auth: None,
                    oauth_parameters: Some(oauth_params("at-nested", "rt-nested")),
                },
                form_parameters: None,
                http_header_fields: vec![],
            }),
            oauth_parameters: None,
        }));
        let (access, refresh, scope) = extract_oauth_params(Some(&server)).unwrap();
        assert_eq!(access, "at-nested");
        assert_eq!(refresh, "rt-nested");
        assert!(scope.is_none());
    }

    #[test]
    fn extract_oauth_params_returns_none_when_no_credentials() {
        assert!(extract_oauth_params(Some(&empty_server(None))).is_none());
        assert!(extract_oauth_params(None).is_none());
    }

    // ── parse_oauth_error_code ───────────────────────────────────────

    #[test]
    fn parse_oauth_error_code_extracts_invalid_grant() {
        let body = br#"{"error":"invalid_grant","error_description":"Token revoked"}"#;
        assert_eq!(
            parse_oauth_error_code(body).as_deref(),
            Some("invalid_grant")
        );
    }

    #[test]
    fn parse_oauth_error_code_extracts_other_codes() {
        let body = br#"{"error":"invalid_request"}"#;
        assert_eq!(
            parse_oauth_error_code(body).as_deref(),
            Some("invalid_request")
        );
    }

    #[test]
    fn parse_oauth_error_code_returns_none_without_error_field() {
        let body = br#"{"access_token":"ok"}"#;
        assert!(parse_oauth_error_code(body).is_none());
    }

    #[test]
    fn parse_oauth_error_code_returns_none_on_malformed_json() {
        assert!(parse_oauth_error_code(b"not json").is_none());
        assert!(parse_oauth_error_code(b"").is_none());
    }
}
