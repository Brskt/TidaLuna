//! Tests for `src/ui/token_filter/mod.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn opaque_is_none_on_entropy_failure() {
    assert_eq!(generate_opaque_with(|_| false), None);
}

#[test]
fn opaque_has_prefix_and_hex_bytes() {
    let o = generate_opaque_with(|buf| {
        buf.fill(0xab);
        true
    })
    .expect("RNG ok");
    assert!(o.starts_with(OPAQUE_PREFIX));
    let hex = &o[OPAQUE_PREFIX.len()..];
    assert_eq!(hex.len(), 32);
    assert_eq!(hex, "ab".repeat(16));
}

#[test]
fn auto_injection_and_rewrite_share_the_api_host_set() {
    let u = |s: &str| RequestUrl::new(s.to_string());
    // The 5 API hosts must be recognised by both predicates (they delegate to
    // nav::is_tidal_api_host - pins the consolidation against drift).
    for url in [
        "https://api.tidal.com/v1/x",
        "https://api.tidalhifi.com/v1/x",
        "https://listen.tidal.com/x",
        "https://desktop.tidal.com/x",
        "https://openapi.tidal.com/x",
    ] {
        assert!(needs_auto_injection(&u(url)), "auto-inject: {url}");
        assert!(should_rewrite_token(&u(url)), "rewrite: {url}");
    }
    // auth/login are rewrite-only (not auto-injected).
    assert!(should_rewrite_token(&u(
        "https://auth.tidal.com/oauth2/token"
    )));
    assert!(!needs_auto_injection(&u(
        "https://auth.tidal.com/oauth2/token"
    )));
    // http scheme is rejected by both.
    assert!(!needs_auto_injection(&u("http://api.tidal.com/x")));
    assert!(!should_rewrite_token(&u("http://api.tidal.com/x")));
}
