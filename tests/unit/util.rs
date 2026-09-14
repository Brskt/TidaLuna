//! Tests for `src/util/mod.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn redact_url_query_strips_query() {
    assert_eq!(
        redact_url_query("https://desktop.tidal.com/login/auth?code=abc123&state=xyz"),
        "https://desktop.tidal.com/login/auth?<redacted>"
    );
}

#[test]
fn redact_url_query_strips_fragment() {
    assert_eq!(
        redact_url_query("https://desktop.tidal.com/cb#access_token=abc123"),
        "https://desktop.tidal.com/cb?<redacted>"
    );
}

#[test]
fn redact_url_query_passthrough_without_query() {
    assert_eq!(
        redact_url_query("https://desktop.tidal.com/browse"),
        "https://desktop.tidal.com/browse"
    );
}

/// The url reaches a refusal log from an untrusted source and `verr!` is never level-gated:
/// logging it verbatim let a refused-call loop write attacker-chosen text (newlines, terminal
/// escapes) into the persistent log at LOGS=0. Only the host is kept, and only bounded.
#[test]
fn a_refused_url_is_reduced_to_a_bounded_host_for_the_log() {
    let forged = "https://evil.test/x\n2026-08-01 [AUTH] token=deadbeef";
    let logged = refused_host(forged);
    assert_eq!(logged, "evil.test");
    assert!(!logged.contains('\n'));

    let long = format!("https://{}.test/x", "a".repeat(300));
    assert!(refused_host(&long).len() <= 64);

    assert_eq!(refused_host("not a url"), "unparseable url");
}

/// A query is where a credential rides: the opaque `luna_*` nonce the renderer holds, an OAuth
/// `code`. Stripping only the length, as `truncate_str` does, leaves a 37-byte nonce whole.
#[test]
fn a_refused_url_carries_none_of_its_query_into_the_log() {
    assert_eq!(
        refused_host("https://evil.test/collect?t=luna_0123456789abcdef0123456789abcdef"),
        "evil.test"
    );
}

/// `reqwest::Error`'s own `Display` appends the request URL, query included, and a media URL's
/// query is a time-bounded CDN credential. This text does not stay in a log: it rides
/// `MediaError` into the renderer's JS realm, into TIDAL's telemetry, and out over the Connect
/// socket to every controller on the network. The URL has to go at the string's origin.
#[tokio::test]
async fn a_network_error_carries_no_request_url() {
    let err = reqwest::get("http://127.0.0.1:1/seg.mp4?token=SECRET&exp=1")
        .await
        .expect_err("port 1 refuses the connection");

    let text = network_error_text(err);
    assert!(!text.contains("SECRET"), "the credential survived: {text}");
    assert!(!text.contains("127.0.0.1"), "the url survived: {text}");
    assert!(!text.is_empty(), "the failure still has to say something");
}

#[test]
fn managed_install_detects_truthy_values() {
    assert!(managed_install_from(Some("1")));
    assert!(managed_install_from(Some("true")));
}

#[test]
fn managed_install_rejects_unset_and_falsy() {
    assert!(!managed_install_from(None));
    assert!(!managed_install_from(Some("")));
    assert!(!managed_install_from(Some("0")));
}
