//! Tests for `src/ipc/plugin/proxy/scrub.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn scrub_replaces_real_token_with_opaque() {
    let pairs = vec![(
        "real-access-token-1234".to_string(),
        "luna_aaaa".to_string(),
    )];
    let body = r#"{"leaked":"real-access-token-1234"}"#.to_string();
    let out = scrub_real_tokens_with(body, &pairs);
    assert!(!out.contains("real-access-token-1234"), "{out}");
    assert!(out.contains("luna_aaaa"), "{out}");
}
