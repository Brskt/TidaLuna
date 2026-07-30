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
