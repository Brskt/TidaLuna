//! Tests for `src/app_state.rs`, attached to it by `#[path]`.

use super::{js_ipc_response, js_string_literal};

#[test]
fn js_string_literal_escapes_quotes_backslashes_and_newlines() {
    assert_eq!(js_string_literal("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
}

#[test]
fn js_string_literal_escapes_line_and_paragraph_separators() {
    // serde_json leaves U+2028/U+2029 raw (RFC 8259), but they are JS line
    // terminators; \u-escape them so the literal stays valid in any position.
    assert_eq!(js_string_literal("a\u{2028}b"), "\"a\\u2028b\"");
    assert_eq!(js_string_literal("a\u{2029}b"), "\"a\\u2029b\"");
}

#[test]
fn js_ipc_response_keeps_malicious_id_inside_one_string_literal() {
    // The audit payload tries to break out of both quote styles.
    let malicious = "x\");evil();//' or '";
    let js = js_ipc_response(malicious, "[1,2]");
    assert!(js.starts_with("window.__TIDAL_IPC_RESPONSE__("));
    assert!(js.ends_with(", null, [1,2])"));
    // The id argument must be a single JSON string literal that decodes back
    // to the exact input - proving it can never escape into code position.
    let first_arg = js
        .strip_prefix("window.__TIDAL_IPC_RESPONSE__(")
        .and_then(|s| s.strip_suffix(", null, [1,2])"))
        .expect("response structure intact");
    let decoded: String =
        serde_json::from_str(first_arg).expect("first arg is a valid JSON string literal");
    assert_eq!(decoded, malicious);
}
