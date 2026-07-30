//! Tests for `src/connect/ipc/helpers.rs`, attached to it by `#[path]`.

use super::build_emit_js;

#[test]
fn build_emit_js_keeps_channel_in_one_string_literal() {
    // A backslash before a quote defeats a single-quote-only escape; the
    // channel must stay one inert JS string literal that round-trips.
    let evil = "x\\';alert(1)//";
    let js = build_emit_js(evil, None);
    let inner = js
        .strip_prefix("if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__(")
        .and_then(|s| s.strip_suffix(");"))
        .expect("emit structure intact");
    let decoded: String =
        serde_json::from_str(inner).expect("channel is a valid JSON string literal");
    assert_eq!(decoded, evil);
}
