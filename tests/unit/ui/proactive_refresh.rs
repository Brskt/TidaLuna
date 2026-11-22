//! Tests for `src/ui/proactive_refresh.rs`, attached by `#[path]`.

use super::RefreshVerdict;

/// Only `400 invalid_grant` says the refresh token is dead, and only `401 invalid_client` that its
/// client is refused while it lives: an outage, a malformed request or an unreadable body erases
/// nothing. A 200 counts only with an access token in it.
#[test]
fn a_refresh_answer_is_dead_refused_or_fresh_only_as_its_status_and_body_say() {
    use RefreshVerdict::{ClientRefused, Dead, Fresh, Inconclusive};
    for (status, body, expected) in [
        (200, r#"{"access_token":"at","expires_in":3600}"#, Fresh),
        (200, r#"{"expires_in":3600}"#, Inconclusive),
        (200, r#"{"access_token":""}"#, Inconclusive),
        (200, "not json", Inconclusive),
        (
            400,
            r#"{"status":400,"sub_status":1005,"error":"invalid_grant"}"#,
            Dead,
        ),
        (
            401,
            r#"{"status":401,"sub_status":1005,"error":"invalid_client","error_description":"This token was issued to another client"}"#,
            ClientRefused,
        ),
        (401, r#"{"error":"invalid_grant"}"#, Inconclusive),
        (400, r#"{"error":"invalid_request"}"#, Inconclusive),
        (
            400,
            r#"{"error_description":"invalid_grant"}"#,
            Inconclusive,
        ),
        (503, "", Inconclusive),
    ] {
        assert_eq!(
            RefreshVerdict::classify(status, body),
            expected,
            "{status} {body}"
        );
    }
}
