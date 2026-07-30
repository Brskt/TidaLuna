//! Tests for `src/ui/dialog.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn escape_html_covers_the_five_html_metacharacters() {
    assert_eq!(
        escape_html(r#"<a href="x">b & c</a>"#),
        "&lt;a href=&quot;x&quot;&gt;b &amp; c&lt;/a&gt;"
    );
    assert_eq!(escape_html("plain"), "plain");
}
