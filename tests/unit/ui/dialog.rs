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

#[test]
fn a_dialog_fetches_nothing_but_https() {
    use crate::ui::nav::RequestUrl;
    let u = |s: &str| RequestUrl::new(s.to_string());

    assert!(dialog_subresource_allowed(&u(
        "https://avatars.githubusercontent.com/u/1?v=4"
    )));
    // An allowed host changes nothing: the transport is what is judged here.
    assert!(!dialog_subresource_allowed(&u(
        "http://avatars.githubusercontent.com/u/1?v=4"
    )));
    assert!(!dialog_subresource_allowed(&u(
        "http://127.0.0.1:3000/a.png"
    )));
    assert!(!dialog_subresource_allowed(&u("file:///C:/a.png")));
    assert!(!dialog_subresource_allowed(&u("not a url")));
}
