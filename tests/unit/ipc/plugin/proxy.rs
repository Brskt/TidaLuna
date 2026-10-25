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

/// Truncating before the scrub leaks. The cut hands the scrubber a fragment it cannot match,
/// and every substitution ahead of that fragment shortens the string, sliding it left into the
/// window that gets logged. Widening the cut by a token length does not help: the widening
/// covers the token crossing the outer cut, the shrinkage carries a later one into view.
#[test]
fn a_second_token_does_not_slide_into_the_log_window_behind_the_first() {
    let t1 = format!("t1{}", "a".repeat(998));
    let t2 = format!("t2{}", "b".repeat(998));
    // t2 starts at 1200; a 400-byte window widened by one 1000-byte token admits only its
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
fn token_body_empties_on_entropy_failure_never_leaks() {
    // A real-token response with opaque generation failing must return an
    // empty JSON body, never the real token, to plugin JS.
    let body = r#"{"access_token":"real-secret","refresh_token":"real-rt"}"#;
    // The session epoch only gates the commit into `AppState`, which no test reaches: the
    // scrub asserted below happens either way.
    let out = proxy_transform_token_body_with(body, 200, 0, || None);
    assert_eq!(out, "{}");
    assert!(!out.contains("real-secret"));
}

#[test]
fn upstream_body_without_video_artwork_is_returned_verbatim() {
    for body in [
        r#"{"data":{"type":"albums","id":"1"}}"#,
        "<html>not json</html>",
    ] {
        assert_eq!(UpstreamBody(body.to_string()).strip_video_artwork().0, body);
    }
}
