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
