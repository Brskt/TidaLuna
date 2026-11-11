//! Tests for `src/ui/dialog/html.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn escape_html_covers_the_five_html_metacharacters() {
    assert_eq!(
        escape_html(r#"<a href="x">b & c</a>"#),
        "&lt;a href=&quot;x&quot;&gt;b &amp; c&lt;/a&gt;"
    );
    assert_eq!(escape_html("plain"), "plain");
}

#[test]
fn plugin_text_keeps_only_bare_b_and_i_that_pair_up() {
    let cases = [
        (
            "if there is a <b>HiRes</b> version",
            "if there is a <b>HiRes</b> version",
        ),
        ("<i>a <b>b</b></i>", "<i>a <b>b</b></i>"),
        (
            r#"<b class="x">a</b> <u>c</u> & <script>"#,
            "&lt;b class=&quot;x&quot;&gt;a&lt;/b&gt; &lt;u&gt;c&lt;/u&gt; &amp; &lt;script&gt;",
        ),
        // Left open, a tag would carry into the dialog's next blocks.
        ("<b>open", "&lt;b&gt;open"),
        ("shut</i>", "shut&lt;/i&gt;"),
        ("<b><i>x</b></i>", "&lt;b&gt;<i>x&lt;/b&gt;</i>"),
    ];
    for (input, expected) in cases {
        assert_eq!(emphasis_html(input), expected, "{input}");
    }
}
