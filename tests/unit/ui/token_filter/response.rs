//! Tests for `src/ui/token_filter/response.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn token_response_drops_on_entropy_failure_never_passthrough() {
    // A real-token response with opaque generation failing must DROP
    // (Error), never Passthrough, or the real token reaches the renderer.
    let body = br#"{"access_token":"real-secret","refresh_token":"real-rt"}"#;
    assert!(matches!(
        process_token_response_with(body, None, 0, || None),
        ProcessResult::Error
    ));
}

/// A new refresh token binds the client id of its own exchange, never the stale prior: an
/// authorization_code after a client_credentials generation adopts "user", not "app". Without a
/// new refresh token, or with no client id observed, the prior is kept rather than blanked.
#[test]
fn a_generation_takes_the_exchange_client_id_only_with_a_new_refresh_token() {
    for (has_new_refresh_token, exchange, prior, expected) in [
        (true, Some("user"), Some("app"), "user"),
        (false, Some("app"), Some("user"), "user"),
        (true, None, Some("user"), "user"),
        (true, Some(""), Some("user"), "user"),
        (true, Some("user"), None, "user"),
        (false, Some("app"), None, "app"),
    ] {
        assert_eq!(
            resolve_generation_client_id(has_new_refresh_token, exchange, prior),
            expected,
            "new refresh: {has_new_refresh_token}, exchange: {exchange:?}, prior: {prior:?}"
        );
    }
}
