//! Tests for `src/platform/secure_store.rs`, attached to it by `#[path]`.
//!
//! The storage halves reach a real Keychain, DPAPI or a file: they are not
//! exercised here. What is testable, and what a defect turned on, is the rule
//! deciding whether a generation is worth storing at all.

use super::*;

/// A generation shaped like the ones both token paths mint. The refresh pair is
/// the caller's to set, being the subject here.
fn generation(refresh_token: &str, opaque_rt: &str) -> TokenGeneration {
    TokenGeneration {
        access_token: "real-access-token".to_owned(),
        refresh_token: refresh_token.to_owned(),
        opaque_at: "luna_aaaa".to_owned(),
        opaque_rt: opaque_rt.to_owned(),
        version: 1,
        access_expires: 0,
        user_id: None,
        granted_scopes: Vec::new(),
        client_id: "client".to_owned(),
    }
}

#[test]
fn a_generation_carrying_a_refresh_token_is_durable() {
    assert!(generation("real-refresh-token", "luna_bbbb").is_durable());
}

#[test]
fn a_generation_without_a_refresh_token_is_not_durable() {
    // Exactly what both token paths mint when a response carries no
    // refresh_token and no prior generation is in memory to reuse one from: the
    // `(String::new(), String::new())` arm. It serves the running session from
    // memory and is worth nothing on disk, because nothing in it outlives the
    // access token's expiry.
    assert!(!generation("", "").is_durable());
}

#[test]
fn an_opaque_refresh_nonce_alone_does_not_make_a_generation_durable() {
    // The renderer only ever sees the opaque nonce; a generation could carry
    // one while the real token behind it is gone. Storing that would hand the
    // next launch a credential it cannot redeem.
    assert!(!generation("", "luna_bbbb").is_durable());
}
