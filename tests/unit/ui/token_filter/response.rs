//! Tests for `src/ui/token_filter/response.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn token_response_drops_on_entropy_failure_never_passthrough() {
    // A real-token response with opaque generation failing must DROP
    // (Error), never Passthrough, or the real token reaches the renderer.
    let body = br#"{"access_token":"real-secret","refresh_token":"real-rt"}"#;
    assert!(matches!(
        process_token_response_with(body, ExchangeOrigin::Code, None, 0, || None),
        ProcessResult::Error
    ));
}

fn verdict(body: &[u8], origin: ExchangeOrigin) -> &'static str {
    // A failing generator stops a live answer before it commits: no global state is touched.
    match process_token_response_with(body, origin, None, 0, || None) {
        ProcessResult::Modified(_) => "modified",
        ProcessResult::Passthrough => "passthrough",
        ProcessResult::Error => "dropped",
    }
}

/// An inert exchange never hands the page a token: an answer carrying one is dropped, an error
/// body passes so the SDK sees its refusal.
#[test]
fn an_inert_answer_carrying_a_token_is_dropped_and_its_errors_pass() {
    let mut not_utf8 = br#"{"access_token":"real","x":""#.to_vec();
    not_utf8.extend_from_slice(b"\xff\"}");
    for (body, expected) in [
        (
            br#"{"access_token":"real","expires_in":3600}"#.to_vec(),
            "dropped",
        ),
        (br#"{"refresh_token":"real"}"#.to_vec(), "dropped"),
        (not_utf8, "dropped"),
        (
            br#"{"error":"invalid_grant","sub_status":1002}"#.to_vec(),
            "passthrough",
        ),
        (
            br#"{"access_token":"","refresh_token":""}"#.to_vec(),
            "passthrough",
        ),
    ] {
        assert_eq!(
            verdict(&body, ExchangeOrigin::Inert),
            expected,
            "{}",
            String::from_utf8_lossy(&body)
        );
    }
}

/// A leading BOM is what `Response.json()` drops: the answer gets the verdict it has without one,
/// never a passthrough that hands the page the token.
#[test]
fn a_leading_bom_changes_no_verdict() {
    let body = br#"{"access_token":"real","refresh_token":"rt"}"#;
    let mut with_bom = "\u{FEFF}".as_bytes().to_vec();
    with_bom.extend_from_slice(body);
    assert_eq!(verdict(&with_bom, ExchangeOrigin::Inert), "dropped");
}
