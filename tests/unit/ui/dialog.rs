//! Tests for `src/ui/dialog/mod.rs`, attached to it by `#[path]`.

use super::*;

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
