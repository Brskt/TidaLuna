//! Tests for `src/ui/trust_dialog.rs`, attached to it by `#[path]`.

use super::*;

/// Only Validate's answers count as answers; anything else under the scheme ends the dialog as
/// closed, which records nothing, and never as an allow.
#[test]
fn parse_trust_reads_one_answer_per_row_or_nothing() {
    for (url, expected) in [
        (
            "trust://submit/101",
            Some(TrustReply::Answered(vec![true, false, true])),
        ),
        ("trust://submit/0/", Some(TrustReply::Answered(vec![false]))),
        ("trust://submit/1x", Some(TrustReply::Closed)),
        ("trust://submit/", Some(TrustReply::Closed)),
        ("trust://allow", None),
        ("https://desktop.tidal.com/", None),
    ] {
        assert_eq!(parse_trust(url), expected, "{url}");
    }
}

#[test]
fn build_html_lists_each_row_and_escapes() {
    let html = build_html(
        "Plug<in>/x.native.ts",
        &["net", "fs"],
        r#"{"author":{"name":"me & co"}}"#,
    );
    assert!(html.contains("Network Access"));
    assert!(html.contains("Filesystem"));
    assert!(html.contains(r#"name="r1""#));
    assert!(!html.contains(r#"name="r2""#));
    assert!(html.contains("Plug&lt;in&gt;"));
    assert!(html.contains("me &amp; co"));
}
