//! Tests for `src/ui/trust_dialog.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn parse_trust_maps_only_the_two_schemes() {
    assert_eq!(parse_trust(TRUST_ALLOW), Some(true));
    assert_eq!(parse_trust(TRUST_DENY), Some(false));
    assert_eq!(parse_trust("https://desktop.tidal.com/"), None);
    assert_eq!(parse_trust("trust://other"), None);
}

#[test]
fn build_html_wires_button_schemes_and_escapes() {
    let html = build_html("Plug<in>", "fs", r#"{"author":{"name":"me & co"}}"#);
    assert!(html.contains("location.href='trust://allow'"));
    assert!(html.contains("location.href='trust://deny'"));
    assert!(html.contains("Plug&lt;in&gt;"));
    assert!(html.contains("me &amp; co"));
    assert!(html.contains("Filesystem"));
}
