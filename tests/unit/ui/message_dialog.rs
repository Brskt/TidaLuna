//! Tests for `src/ui/message_dialog.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn build_html_indexes_buttons_and_marks_default() {
    let buttons = vec!["OK".to_string(), "Cancel".to_string()];
    let html = build_html("T<>", "msg", "det", &buttons, 0);
    assert!(html.contains("location.href='msgbox://0'"));
    assert!(html.contains("location.href='msgbox://1'"));
    assert!(html.contains(r#"class="btn primary""#));
    assert!(html.contains("T&lt;&gt;"));
}
