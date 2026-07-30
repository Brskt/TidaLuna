//! Tests for `src/ipc/plugin/lib_native.rs`, attached to it by `#[path]`.

use super::build_send_to_render_js;
use serde_json::json;

#[test]
fn send_to_render_escapes_separators_and_encodes_channel() {
    // A string arg containing U+2028 (a JS line terminator) must be escaped so
    // it can't terminate the emit statement and inject the following code.
    let js = build_send_to_render_js("ch", &[json!("a\u{2028}b")]);
    assert!(js.contains("\\u2028"), "U+2028 must be escaped: {js}");
    assert!(!js.contains('\u{2028}'), "raw U+2028 must not remain: {js}");
    // The channel is a JSON string literal that can't break out.
    assert!(js.contains("__LUNAR_IPC_EMIT__(\"ch\","));
}

#[test]
fn send_to_render_no_args_omits_trailing_comma() {
    let js = build_send_to_render_js("ch", &[]);
    assert!(js.ends_with("__LUNAR_IPC_EMIT__(\"ch\");"), "{js}");
}
