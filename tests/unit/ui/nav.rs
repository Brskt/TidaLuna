//! Tests for `src/ui/nav.rs`, attached to it by `#[path]`.

use super::*;

fn u(s: &str) -> RequestUrl {
    RequestUrl::new(s.to_string())
}

/// What does not parse falls back to a relative path, which is same-origin. `//evil.example/x` is
/// a protocol-relative reference to a foreign host that `starts_with('/')` alone cannot tell apart.
#[test]
fn a_tidal_origin_is_the_apex_a_subdomain_or_a_relative_path() {
    for (target, expected) in [
        ("https://tidal.com/", true),
        ("https://listen.tidal.com/v1/x", true),
        ("https://eviltidal.com/", false),
        ("https://tidal.com.evil.io/", false),
        ("//evil.example/x", false),
        ("//tidal.com.evil.io/x", false),
        ("not a url", false),
        ("/v1/tracks/1", true),
    ] {
        assert_eq!(is_tidal_origin(&u(target)), expected, "{target}");
    }
}

/// `proxy.fetch` puts the real refresh token in a token-endpoint body and hands it to reqwest,
/// which has no https_only: a plaintext target never passes. Fail closed: a url that carries no
/// transport (`fetch_proxy.js:57` diverts real bare paths first), or a foreign host, is refused.
#[test]
fn a_secure_tidal_target_is_https_on_a_tidal_origin_and_nothing_else() {
    for (target, expected) in [
        ("http://auth.tidal.com/v1/oauth2/token", false),
        ("http://listen.tidal.com/v1/tracks", false),
        ("http://tidal.com/", false),
        ("https://listen.tidal.com/v1/tracks", true),
        ("https://auth.tidal.com/v1/oauth2/token", true),
        ("//evil.example/x", false),
        ("/v1/tracks/1", false),
        ("https://evil.example/v1/tracks", false),
        ("not a url", false),
    ] {
        assert_eq!(is_secure_tidal_target(&u(target)), expected, "{target}");
    }
}

/// Covers and contributor faces need the apex and its subdomains, behind the dot boundary. TIDAL's
/// legacy API and telemetry hosts clear `should_rewrite_token` without clearing `is_tidal_origin`
/// and serve no image: the hoisted gate refuses them, with lookalikes and what does not parse.
#[test]
fn an_allowed_image_host_is_an_allowed_apex_or_its_subdomain_and_nothing_else() {
    for (target, expected) in [
        ("https://tidal.com/favicon.png", true),
        ("https://resources.tidal.com/images/a/1280x1280.jpg", true),
        ("https://avatars.githubusercontent.com/u/1?v=4", true),
        ("https://images.captcha-delivery.com/challenge.png", true),
        ("https://eviltidal.com/p.png", false),
        ("https://tidal.com.evil.io/p.png", false),
        ("https://evil.example/p.png?ref=tidal.com", false),
        ("https://evil.example/tidal.com/p.png", false),
        ("https://api.tidalhifi.com/x.jpg", false),
        ("https://event-collector.eu.tidalhi.fi/x.gif", false),
        ("not a url", false),
        ("", false),
    ] {
        assert_eq!(is_allowed_image_host(&u(target)), expected, "{target:?}");
    }
}

/// Not every allowed host is HSTS-preloaded: the host alone cannot admit a plaintext URL.
#[test]
fn secure_image_target_needs_an_allowed_host_over_https() {
    assert!(is_secure_image_target(&u(
        "https://resources.tidal.com/images/a/1280x1280.jpg"
    )));
    assert!(!is_secure_image_target(&u(
        "http://resources.tidal.com/images/a/1280x1280.jpg"
    )));
    assert!(!is_secure_image_target(&u(
        "http://images.captcha-delivery.com/challenge.png"
    )));
    assert!(!is_secure_image_target(&u("https://evil.example/p.png")));
    assert!(!is_secure_image_target(&u("not a url")));
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
