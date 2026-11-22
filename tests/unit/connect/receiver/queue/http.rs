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

/// A phone names the servers the receiver sends its token to. A refresh goes to TIDAL's token host
/// alone, every other request to a service host; a host of the wrong kind, or any URL carrying
/// more than a root, is refused. A port is admitted: the receiver tests point a root at their own.
#[test]
fn a_root_is_admitted_for_its_kind_alone() {
    use RootKind::{Service, Token};
    for (raw, service, token) in [
        ("https://api.tidal.com/x", true, false),
        ("https://connectqueue.tidal.com/v1/queues", true, false),
        ("https://api.tidalhifi.com/x", true, false),
        ("https://API.tidal.com/x", true, false),
        ("https://api.tidal.com:8443/x", true, false),
        ("https://auth.tidal.com/oauth", false, true),
        ("https://auth.tidal.com/v1/oauth2/token", false, true),
        ("https://desktop.tidal.com", false, false),
        ("https://login.tidal.com/oauth2/token", false, false),
        ("http://api.tidal.com", false, false),
        ("https://evil.com", false, false),
        ("https://api.tidal.com.evil.com", false, false),
        ("https://api.tidal.com./x", false, false),
        ("https://api.tidal.com@evil.com", false, false),
        ("https://user@api.tidal.com/x", false, false),
        ("https://api.tidal.com/x?redirect=evil", false, false),
        ("https://api.tidal.com/x#frag", false, false),
        ("/relative/path", false, false),
    ] {
        assert_eq!(
            ConnectRoot::admit(raw, Service).is_ok(),
            service,
            "service {raw}"
        );
        assert_eq!(ConnectRoot::admit(raw, Token).is_ok(), token, "token {raw}");
    }
}

/// Every id a phone sends lands in one path segment whatever it holds, or is refused by index; a
/// query value stays one value. The root's own trailing slash changes nothing.
#[test]
fn a_peer_id_stays_one_segment_and_a_query_value_stays_one_value() {
    let at = |raw: &str| ConnectRoot::admit(raw, RootKind::Service).unwrap();
    let root = at("https://connectqueue.tidal.com/v1/queues");
    let uuid = "3f2a8c1e-9b4d-4e6f-a1b2-c3d4e5f60718";
    for (segments, expected) in [
        (
            vec![uuid, "window"],
            Ok(format!("/v1/queues/{uuid}/window")),
        ),
        (vec!["a/b", "window"], Ok("/v1/queues/a%2Fb/window".into())),
        (vec!["a?b", "window"], Ok("/v1/queues/a%3Fb/window".into())),
        (
            vec!["%2e%2e", "window"],
            Ok("/v1/queues/%252e%252e/window".into()),
        ),
        (vec!["..", "window"], Err(0)),
        (vec![".", "window"], Err(0)),
        (vec!["..\r", "window"], Err(0)),
        (vec!["q-1", " \t "], Err(1)),
        (vec!["", "window"], Err(0)),
    ] {
        let built = root
            .url(&segments, &[])
            .map(|u| u.url.path().to_string())
            .map_err(|e| match e {
                QueueError::RefusedSegment(index) => index,
                other => panic!("{segments:?}: {other:?}"),
            });
        assert_eq!(built, expected, "{segments:?}");
    }
    let slashed = at("https://connectqueue.tidal.com/v1/queues/");
    assert_eq!(
        slashed.url(&["q-1", "window"], &[]).unwrap().url.path(),
        "/v1/queues/q-1/window"
    );
    let item = "a&onlyactives=false#x";
    let with_query = root
        .url(
            &["q-1", "window"],
            &[("onlyactives", "true"), ("itemid", item)],
        )
        .unwrap()
        .url;
    let pairs: Vec<(String, String)> = with_query.query_pairs().into_owned().collect();
    assert_eq!(
        pairs,
        vec![
            ("onlyactives".to_string(), "true".to_string()),
            ("itemid".to_string(), item.to_string()),
        ]
    );
    assert!(with_query.fragment().is_none());
}

/// A queue server that redirects off TIDAL's service hosts is refused at the hop, and the phone
/// hears a fixed refusal rather than a network error carrying the URL.
#[tokio::test]
async fn a_redirect_off_the_service_hosts_is_a_refusal_not_a_network_error() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 1024];
        let _ = sock.read(&mut request).await;
        let _ = sock
            .write_all(
                b"HTTP/1.1 302 Found\r\nLocation: https://evil.example/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await;
    });
    let client = crate::state::host_checked_builder(
        crate::state::service_class,
        "redirect left TIDAL's service hosts",
        crate::state::HopCap::Error,
    )
    .build()
    .unwrap();
    let err = client
        .get(format!("http://127.0.0.1:{port}/v1/queues/q-1/window"))
        .send()
        .await
        .expect_err("the hop is refused");
    let refused = send_error(err);
    assert!(
        matches!(refused, QueueError::RedirectRefused),
        "{refused:?}"
    );
    assert_eq!(refused.to_string(), "redirect refused");
}

/// The queue server's refusal of a change once read as a success: every status lands where
/// TIDAL's receiver (`QueueWindowLoader::onReceived`) puts it.
#[test]
fn a_put_reply_is_sorted_by_status_as_tidal_sorts_it() {
    const BODY: &str = r#"{"error":"client_error","error_description":"bad_input"}"#;
    let body = || BODY.to_string();
    for (status, expected) in [
        (200, PutReply::Applied),
        (204, PutReply::Applied),
        (401, PutReply::TokenExpired { body: body() }),
        (
            400,
            PutReply::Refused {
                status: 400,
                body: body(),
            },
        ),
        (
            500,
            PutReply::Refused {
                status: 500,
                body: body(),
            },
        ),
        (201, PutReply::Unhandled { status: 201 }),
        (403, PutReply::Unhandled { status: 403 }),
    ] {
        assert_eq!(PutReply::sort(status, body()), expected, "{status}");
    }
}
