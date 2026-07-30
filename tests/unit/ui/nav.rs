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
