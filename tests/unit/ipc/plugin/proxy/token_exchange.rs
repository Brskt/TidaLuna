//! Tests for `src/ipc/plugin/proxy/token_exchange.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn token_body_empties_on_entropy_failure_never_leaks() {
    // A real-token response with opaque generation failing must return an
    // empty JSON body, never the real token, to plugin JS.
    let body = r#"{"access_token":"real-secret","refresh_token":"real-rt"}"#;
    // The session epoch only gates the commit into `AppState`, which no test reaches: the
    // scrub asserted below happens either way.
    let out = proxy_transform_token_body_with(body, 200, 0, None, || None);
    assert_eq!(out, "{}");
    assert!(!out.contains("real-secret"));
}
