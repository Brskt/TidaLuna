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
fn secure_tidal_target_refuses_plaintext() {
    // THE LEAK. `proxy.fetch` substitutes the real refresh token into a token-endpoint body
    // and hands it to reqwest, which has no https_only and never sees Blink. Worse, Blink
    // blocking the page's own plaintext fetch is what raises the TypeError that sends the
    // request down this path in the first place.
    assert!(!is_secure_tidal_target(&u(
        "http://auth.tidal.com/v1/oauth2/token"
    )));
    assert!(!is_secure_tidal_target(&u(
        "http://listen.tidal.com/v1/tracks"
    )));
    assert!(!is_secure_tidal_target(&u("http://tidal.com/")));
}

#[test]
fn secure_tidal_target_keeps_every_legitimate_caller() {
    assert!(is_secure_tidal_target(&u(
        "https://listen.tidal.com/v1/tracks"
    )));
    assert!(is_secure_tidal_target(&u(
        "https://auth.tidal.com/v1/oauth2/token"
    )));
}

/// Fail closed, like every other predicate in this file. `fetch_proxy.js:57` diverts real bare
/// paths to `nativeFetch` before they reach `proxy.fetch`, and nothing here ever joins one to a
/// base, so the string reaching reqwest is the string this gate read. A transport rule cannot
/// answer for a URL that carries no transport.
#[test]
fn secure_tidal_target_refuses_what_it_cannot_parse() {
    assert!(!is_secure_tidal_target(&u("//evil.example/x")));
    assert!(!is_secure_tidal_target(&u("/v1/tracks/1")));
}

#[test]
fn secure_tidal_target_still_refuses_a_foreign_host() {
    assert!(!is_secure_tidal_target(&u(
        "https://evil.example/v1/tracks"
    )));
    assert!(!is_secure_tidal_target(&u("not a url")));
}

#[test]
fn allowed_image_host_takes_the_apex_and_its_subdomains() {
    // resources.tidal.com serves every cover, avatars.githubusercontent.com every
    // contributor face: exact-host matching would empty the UI.
    assert!(is_allowed_image_host(&u("https://tidal.com/favicon.png")));
    assert!(is_allowed_image_host(&u(
        "https://resources.tidal.com/images/a/1280x1280.jpg"
    )));
    assert!(is_allowed_image_host(&u(
        "https://avatars.githubusercontent.com/u/1?v=4"
    )));
    assert!(is_allowed_image_host(&u(
        "https://images.captcha-delivery.com/challenge.png"
    )));
}

#[test]
fn allowed_image_host_refuses_a_registrable_look_alike() {
    // Both carry the allowed string; the dot boundary is what separates them.
    assert!(!is_allowed_image_host(&u("https://eviltidal.com/p.png")));
    assert!(!is_allowed_image_host(&u(
        "https://tidal.com.evil.io/p.png"
    )));
}

#[test]
fn allowed_image_host_refuses_an_allowed_name_carried_elsewhere_in_the_url() {
    assert!(!is_allowed_image_host(&u(
        "https://evil.example/p.png?ref=tidal.com"
    )));
    assert!(!is_allowed_image_host(&u(
        "https://evil.example/tidal.com/p.png"
    )));
}

/// TIDAL's own legacy API host and its telemetry collectors clear `should_rewrite_token` without
/// clearing `is_tidal_origin`, which used to carry the whole dispatch past this rule. The shipped
/// bundle serves no image from either (artwork comes from `images.tidal.com`/`resources.tidal.com`;
/// telemetry posts by `fetch`, not by pixel), so the allowlist stays as arbitrated and the hoisted
/// gate refuses them.
#[test]
fn allowed_image_host_refuses_tidals_legacy_api_and_telemetry_domains() {
    assert!(!is_allowed_image_host(&u(
        "https://api.tidalhifi.com/x.jpg"
    )));
    assert!(!is_allowed_image_host(&u(
        "https://event-collector.eu.tidalhi.fi/x.gif"
    )));
}

#[test]
fn allowed_image_host_refuses_what_it_cannot_parse() {
    // Fail closed: a string that will not parse says nothing about its target.
    assert!(!is_allowed_image_host(&u("not a url")));
    assert!(!is_allowed_image_host(&u("")));
}

/// `is_secure_tidal_target` admits on `is_tidal_origin` alone, which is only sound while every
/// host `is_token_endpoint` recognises is itself a Tidal origin. That holds today because both
/// constants end in `.tidal.com`, but `is_tidal_origin`'s suffix literal does not derive from
/// them, and nothing else keeps the two in step. Add an auth host on one side only and this turns
/// red, instead of the gate silently 403-ing real token-refresh traffic.
#[test]
fn every_token_endpoint_host_is_also_a_tidal_origin() {
    for host in [HOST_AUTH, HOST_LOGIN] {
        let url = u(&format!("https://{host}/v1/oauth2/token"));
        assert!(
            is_token_endpoint(&url),
            "{host} is meant to be a token endpoint"
        );
        assert!(
            is_tidal_origin(&url),
            "{host} is a token endpoint but not a Tidal origin, so is_secure_tidal_target refuses it"
        );
    }
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
