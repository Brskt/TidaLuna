//! Tests for `src/connect/receiver/queue/http.rs`, attached to it by `#[path]`.

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

// ── is_trusted_server_url ────────────────────────────────────────

#[test]
fn trusted_server_url_requires_https_and_tidal_host() {
    assert!(is_trusted_server_url("https://api.tidal.com/x"));
    assert!(is_trusted_server_url("https://desktop.tidal.com"));
    assert!(is_trusted_server_url("https://auth.tidal.com/oauth"));
    assert!(is_trusted_server_url("https://tidal.com"));
    // tidalhifi.com is a TIDAL-owned API family the rest of the app trusts
    assert!(is_trusted_server_url("https://api.tidalhifi.com/x"));
    assert!(is_trusted_server_url("https://tidalhifi.com"));
    // scheme must be https
    assert!(!is_trusted_server_url("http://api.tidal.com"));
    // non-tidal host
    assert!(!is_trusted_server_url("https://evil.com"));
    // suffix / look-alike tricks
    assert!(!is_trusted_server_url("https://api.tidal.com.evil.com"));
    assert!(!is_trusted_server_url("https://eviltidal.com"));
    assert!(!is_trusted_server_url("https://api.tidalhifi.com.evil.com"));
    assert!(!is_trusted_server_url("https://xtidalhifi.com"));
    // userinfo trick: the real host is evil.com
    assert!(!is_trusted_server_url("https://api.tidal.com@evil.com"));
    // not an absolute URL
    assert!(!is_trusted_server_url("/relative/path"));
}
