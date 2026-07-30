//! Tests for `src/updater/verify.rs`, attached to it by `#[path]`.

use super::verify_with_key;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signer, SigningKey};

fn sign(seed: [u8; 32], msg: &[u8]) -> ([u8; 32], String) {
    let sk = SigningKey::from_bytes(&seed);
    let sig = sk.sign(msg);
    (sk.verifying_key().to_bytes(), BASE64.encode(sig.to_bytes()))
}

#[test]
fn accepts_a_valid_signature() {
    let (key, sig_b64) = sign([7u8; 32], b"manifest");
    assert!(verify_with_key(&key, b"manifest", &sig_b64).is_ok());
}

#[test]
fn rejects_a_tampered_manifest() {
    let (key, sig_b64) = sign([7u8; 32], b"manifest");
    assert!(verify_with_key(&key, b"manifest-TAMPERED", &sig_b64).is_err());
}

#[test]
fn rejects_a_wrong_or_malformed_signature() {
    let (key, _) = sign([7u8; 32], b"manifest");
    let (_other, wrong) = sign([9u8; 32], b"manifest");
    assert!(verify_with_key(&key, b"manifest", &wrong).is_err());
    assert!(verify_with_key(&key, b"manifest", "!!notb64!!").is_err());
}
