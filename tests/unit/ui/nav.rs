//! Tests for `src/ui/nav.rs`, attached to it by `#[path]`.

use super::*;

fn u(s: &str) -> RequestUrl {
    RequestUrl::new(s.to_string())
}

#[test]
fn tidal_origin_matches_the_apex_and_subdomains() {
    assert!(is_tidal_origin(&u("https://tidal.com/")));
    assert!(is_tidal_origin(&u("https://listen.tidal.com/v1/x")));
    assert!(!is_tidal_origin(&u("https://eviltidal.com/")));
    assert!(!is_tidal_origin(&u("https://tidal.com.evil.io/")));
}

#[test]
fn tidal_origin_keeps_the_relative_path_fallback() {
    assert!(is_tidal_origin(&u("/v1/tracks/1")));
    assert!(!is_tidal_origin(&u("not a url")));
}

/// `//evil.example/x` is a protocol-relative absolute reference, not a same-origin path, and
/// `starts_with('/')` alone cannot tell the two apart. `fetch_proxy.js:57` already draws the
/// line on the JS side; this fallback did not, so it answered same-origin for a foreign host.
#[test]
fn tidal_origin_refuses_a_protocol_relative_url() {
    assert!(!is_tidal_origin(&u("//evil.example/x")));
    assert!(!is_tidal_origin(&u("//tidal.com.evil.io/x")));
    assert!(is_tidal_origin(&u("/v1/tracks/1")));
}

#[test]
fn token_endpoint_requires_auth_host_and_oauth_path() {
    assert!(is_token_endpoint(&u(
        "https://auth.tidal.com/v1/oauth2/token"
    )));
    assert!(is_token_endpoint(&u(
        "https://login.tidal.com/oauth2/token"
    )));
    assert!(!is_token_endpoint(&u("https://auth.tidal.com/v1/other")));
    assert!(!is_token_endpoint(&u(
        "https://api.tidal.com/v1/oauth2/token"
    )));
    assert!(!is_token_endpoint(&u("not a url")));
}
