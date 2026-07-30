use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

// Ed25519 public key for update manifests. Keep in sync with the same const in
// updater/src/main.rs, which re-verifies before the privileged swap.
const UPDATE_PUBLIC_KEY: [u8; 32] = [
    104, 175, 158, 150, 215, 73, 36, 25, 193, 27, 127, 255, 238, 170, 136, 130, 171, 47, 180, 243,
    2, 222, 95, 197, 57, 244, 218, 25, 117, 200, 42, 57,
];

/// Verify the Ed25519 signature (`sig`, raw UTF-8 base64) over `manifest_bytes`.
/// The pre-downloader stages files off the manifest, so reject a tampered or
/// unsigned manifest before any of it drives I/O.
pub(super) fn verify_manifest_signature(manifest_bytes: &[u8], sig: &[u8]) -> Result<()> {
    let sig_b64 = std::str::from_utf8(sig).context("signature is not UTF-8")?;
    verify_with_key(&UPDATE_PUBLIC_KEY, manifest_bytes, sig_b64)
}

fn verify_with_key(key: &[u8; 32], manifest_bytes: &[u8], sig_b64: &str) -> Result<()> {
    let sig_bytes = BASE64
        .decode(sig_b64.trim())
        .context("invalid base64 in signature")?;
    let signature =
        Signature::from_slice(&sig_bytes).context("invalid Ed25519 signature format")?;
    let verifying_key = VerifyingKey::from_bytes(key).context("invalid embedded public key")?;
    verifying_key
        .verify(manifest_bytes, &signature)
        .context("manifest signature verification failed")?;
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/updater/verify.rs"]
mod tests;
