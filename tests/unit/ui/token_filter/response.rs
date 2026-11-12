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
