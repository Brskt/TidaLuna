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
fn only_the_manifest_signed_by_the_key_verifies() {
    let (key, signature) = sign([7u8; 32], b"manifest");
    let (_other, wrong) = sign([9u8; 32], b"manifest");
    let cases: [(&str, &[u8], &str, bool); 4] = [
        ("valid", b"manifest", &signature, true),
        ("tampered manifest", b"manifest-TAMPERED", &signature, false),
        ("signed by another key", b"manifest", &wrong, false),
        ("malformed base64", b"manifest", "!!notb64!!", false),
    ];
    for (what, manifest, sig_b64, valid) in cases {
        assert_eq!(
            verify_with_key(&key, manifest, sig_b64).is_ok(),
            valid,
            "{what}"
        );
    }
}
