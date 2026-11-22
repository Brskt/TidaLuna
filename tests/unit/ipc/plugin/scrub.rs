//! Tests for `src/ipc/plugin/scrub.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn a_real_token_in_a_body_is_replaced_by_its_nonce() {
    let pairs = vec![(
        "real-access-token-1234".to_string(),
        "luna_aaaa".to_string(),
    )];
    let body = r#"{"leaked":"real-access-token-1234"}"#.to_string();
    let out = scrub_real_tokens_with(body, &pairs);
    assert!(!out.contains("real-access-token-1234"), "{out}");
    assert!(out.contains("luna_aaaa"), "{out}");
}
