//! Tests for `src/ipc/plugin/proxy.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn scrub_replaces_real_token_with_opaque() {
    let pairs = vec![(
        "real-access-token-1234".to_string(),
        "luna_aaaa".to_string(),
    )];
    let body = r#"{"leaked":"real-access-token-1234"}"#.to_string();
    let out = scrub_real_tokens_with(body, &pairs);
    assert!(!out.contains("real-access-token-1234"), "{out}");
    assert!(out.contains("luna_aaaa"), "{out}");
}

#[test]
fn scrub_leaves_clean_body_untouched() {
    let pairs = vec![(
        "real-access-token-1234".to_string(),
        "luna_aaaa".to_string(),
    )];
    let body = r#"{"tracks":[1,2,3]}"#.to_string();
    assert_eq!(scrub_real_tokens_with(body.clone(), &pairs), body);
}

#[test]
fn scrub_ignores_short_tokens() {
    // A short value must not substring-match and corrupt the body.
    let pairs = vec![("abc".to_string(), "X".to_string())];
    let body = "abcdef".to_string();
    assert_eq!(scrub_real_tokens_with(body.clone(), &pairs), body);
}

#[test]
fn redacted_marker_is_not_an_opaque_nonce() {
    // The no-opaque fallback must not pass is_opaque(): if it were echoed back
    // as a Bearer, rewrite_authorization_header would treat it as a real nonce.
    assert!(!crate::ui::token_filter::is_opaque(REDACTED_MARKER));
}

#[test]
fn token_body_empties_on_entropy_failure_never_leaks() {
    // A real-token response with opaque generation failing must return an
    // empty JSON body, never the real token, to plugin JS.
    let body = r#"{"access_token":"real-secret","refresh_token":"real-rt"}"#;
    let out = proxy_transform_token_body_with(body, 200, || None);
    assert_eq!(out, "{}");
    assert!(!out.contains("real-secret"));
}
