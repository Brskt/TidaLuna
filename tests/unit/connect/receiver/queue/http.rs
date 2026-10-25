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

/// A populated `header_auth` is returned verbatim, even beside outer oauth_parameters: the
/// literal header may carry schemes or signatures the plain access_token cannot rebuild.
#[test]
fn resolve_auth_header_takes_the_first_credential_the_auth_info_carries() {
    let nested = OAuthServerInfo {
        server_url: "https://auth.tidal.com".into(),
        auth_info: OAuthAuthInfo {
            header_auth: None,
            oauth_parameters: Some(oauth_params("at-nested", "rt-nested")),
        },
        form_parameters: None,
        http_header_fields: vec![],
    };
    for (server, expected) in [
        (
            empty_server(Some(AuthInfo {
                header_auth: Some("Bearer from-header".into()),
                oauth_server_info: None,
                oauth_parameters: Some(oauth_params("at-outer", "rt-outer")),
            })),
            "Bearer from-header",
        ),
        (
            empty_server(Some(AuthInfo {
                header_auth: None,
                oauth_server_info: None,
                oauth_parameters: Some(oauth_params("at-outer", "rt-outer")),
            })),
            "Bearer at-outer",
        ),
        (
            empty_server(Some(AuthInfo {
                header_auth: None,
                oauth_server_info: Some(nested),
                oauth_parameters: None,
            })),
            "Bearer at-nested",
        ),
        (empty_server(None), ""),
    ] {
        assert_eq!(resolve_auth_header(&server), expected, "{expected:?}");
    }
}

#[test]
fn extract_oauth_params_prefers_outer_then_nested_and_none_without_credentials() {
    let nested = |form_parameters| OAuthServerInfo {
        server_url: "https://auth".into(),
        auth_info: OAuthAuthInfo {
            header_auth: None,
            oauth_parameters: Some(oauth_params("at-nested", "rt-nested")),
        },
        form_parameters,
        http_header_fields: vec![],
    };
    for (what, server, expected) in [
        (
            "outer beside nested",
            Some(empty_server(Some(AuthInfo {
                header_auth: None,
                oauth_server_info: Some(nested(Some(OAuthFormParameters {
                    grant_type: "refresh_token".into(),
                    scope: "r_usr".into(),
                }))),
                oauth_parameters: Some(oauth_params("at-outer", "rt-outer")),
            }))),
            Some(("at-outer", "rt-outer", Some("r_usr"))),
        ),
        (
            "nested alone",
            Some(empty_server(Some(AuthInfo {
                header_auth: None,
                oauth_server_info: Some(nested(None)),
                oauth_parameters: None,
            }))),
            Some(("at-nested", "rt-nested", None)),
        ),
        ("no auth info", Some(empty_server(None)), None),
        ("no server", None, None),
    ] {
        let found = extract_oauth_params(server.as_ref());
        assert_eq!(
            found.as_ref().map(|(access, refresh, scope)| (
                access.as_str(),
                refresh.as_str(),
                scope.as_deref()
            )),
            expected,
            "{what}"
        );
    }
}

#[test]
fn parse_oauth_error_code_reads_the_error_field_and_nothing_else() {
    for (body, expected) in [
        (
            r#"{"error":"invalid_grant","error_description":"Token revoked"}"#,
            Some("invalid_grant"),
        ),
        (r#"{"error":"invalid_request"}"#, Some("invalid_request")),
        (r#"{"access_token":"ok"}"#, None),
        ("not json", None),
        ("", None),
    ] {
        assert_eq!(
            parse_oauth_error_code(body.as_bytes()).as_deref(),
            expected,
            "{body:?}"
        );
    }
}

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
