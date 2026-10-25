//! Tests for `src/ui/install_dialog.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn build_html_shows_the_fetched_name_and_source_escaped() {
    let html = build_html(
        "Plug<in>",
        r#"https://store.example/"><b>x"#,
        r#"{"author":{"name":"me & co"}}"#,
    );
    assert!(html.contains("location.href='install://allow'"));
    assert!(html.contains("location.href='install://deny'"));
    assert!(html.contains("Plug&lt;in&gt;"));
    assert!(html.contains("https://store.example/&quot;&gt;&lt;b&gt;x"));
    assert!(!html.contains("<b>x"));
    assert!(html.contains("me &amp; co"));
}
