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
fn rewrite_covers_an_api_host_and_the_auth_host() {
    let u = |s: &str| RequestUrl::new(s.to_string());
    assert!(should_rewrite_token(&u("https://api.tidal.com/v1/x")));
    assert!(should_rewrite_token(&u(
        "https://auth.tidal.com/oauth2/token"
    )));
}
