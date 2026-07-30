//! Tests for `src/ui/message_dialog.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn parse_msgbox_parses_index_and_rejects_junk() {
    assert_eq!(parse_msgbox("msgbox://0"), Some(0));
    assert_eq!(parse_msgbox("msgbox://3"), Some(3));
    assert_eq!(parse_msgbox("msgbox://x"), None);
    assert_eq!(parse_msgbox("msgbox://"), None);
    assert_eq!(parse_msgbox("https://desktop.tidal.com/"), None);
}

#[test]
fn build_html_indexes_buttons_and_marks_default() {
    let buttons = vec!["OK".to_string(), "Cancel".to_string()];
    let html = build_html("T<>", "msg", "det", &buttons, 0);
    assert!(html.contains("location.href='msgbox://0'"));
    assert!(html.contains("location.href='msgbox://1'"));
    assert!(html.contains(r#"class="btn primary""#));
    assert!(html.contains("T&lt;&gt;"));
}
