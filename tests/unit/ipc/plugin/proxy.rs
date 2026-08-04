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

/// Truncating before the scrub leaks. The cut hands the scrubber a fragment it cannot match,
/// and every substitution ahead of that fragment shortens the string, sliding it left into the
/// window that gets logged. Widening the cut by a token length does not help: the widening
/// covers the token crossing the outer cut, the shrinkage carries a later one into view.
#[test]
fn a_second_token_does_not_slide_into_the_log_window_behind_the_first() {
    let t1 = format!("t1{}", "a".repeat(998));
    let t2 = format!("t2{}", "b".repeat(998));
    // t2 starts at 1200, so a 400-byte window widened by one 1000-byte token admits only its
    // first 200 bytes: unmatched there, and 990 bytes closer to the front once t1 is replaced.
    let body = format!("{t1}{}{t2}", "-".repeat(200));
    let pairs = vec![
        (t1.clone(), "luna_1".to_string()),
        (t2.clone(), "luna_2".to_string()),
    ];

    let out = UpstreamBody(body).scrubbed_for_log_with(400, &pairs);

    assert!(!out.contains(&t1[..16]), "first token leaked: {out}");
    assert!(!out.contains(&t2[..16]), "second token leaked: {out}");
    assert!(out.contains("luna_1") && out.contains("luna_2"), "{out}");
}

#[test]
fn a_token_straddling_the_log_cut_is_never_emitted_as_a_fragment() {
    let token = format!("tok{}", "c".repeat(97));
    let body = format!("{}{token}", "-".repeat(390));
    let pairs = vec![(token.clone(), "luna_z".to_string())];

    let out = UpstreamBody(body).scrubbed_for_log_with(400, &pairs);

    assert!(!out.contains(&token[..8]), "{out}");
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
