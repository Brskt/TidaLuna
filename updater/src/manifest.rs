use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::Manifest;

// Ed25519 public key for update manifests; CI signs them with its `UPDATE_SIGNING_KEY` secret.
// `cargo xtask generate-keypair` prints a new keypair; src/updater/verify.rs holds the same const.
const UPDATE_PUBLIC_KEY: [u8; 32] = [
    104, 175, 158, 150, 215, 73, 36, 25, 193, 27, 127, 255, 238, 170, 136, 130, 171, 47, 180, 243,
    2, 222, 95, 197, 57, 244, 218, 25, 117, 200, 42, 57,
];

/// Reject absolute paths and directory-escape components (e.g. "..", prefix).
/// Returns true only if `app_dir.join(rel)` resolves to a path under `app_dir`.
pub(crate) fn is_safe_relative_path(rel: &str, app_dir: &Path) -> bool {
    let p = Path::new(rel);
    if p.is_absolute() {
        return false;
    }
    for component in p.components() {
        match component {
            std::path::Component::ParentDir | std::path::Component::Prefix(_) => return false,
            _ => {}
        }
    }
    // Final check: canonicalize-free containment
    app_dir.join(rel).starts_with(app_dir)
}

/// Verify an Ed25519 signature over manifest bytes.
pub(crate) fn verify_manifest_signature(manifest_bytes: &[u8], sig_b64: &str) -> Result<()> {
    let sig_bytes = BASE64
        .decode(sig_b64.trim())
        .context("invalid base64 in signature")?;
    let signature =
        Signature::from_slice(&sig_bytes).context("invalid Ed25519 signature format")?;
    let verifying_key =
        VerifyingKey::from_bytes(&UPDATE_PUBLIC_KEY).context("invalid embedded public key")?;
    verifying_key
        .verify(manifest_bytes, &signature)
        .context("manifest signature verification FAILED - update rejected")?;
    say!("[updater] Manifest signature verified");
    Ok(())
}

pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(base16ct::lower::encode_string(&hasher.finalize()))
}

/// Check the files a delta archive omitted against the hashes the signed manifest records.
///
/// "Unchanged" is settled at release-build time by two manifests agreeing on a hash, which
/// says nothing about the bytes on this machine, and no other step in the update reads the
/// local copy of an omitted file. Reports the first disagreement for the caller to pick
/// between a full-archive retry and failing outright.
pub(crate) fn verify_delta_base(
    manifest: &Manifest,
    app_dir: &Path,
    staging_dir: &Path,
) -> Result<(), String> {
    for (rel_path, entry) in &manifest.files {
        if staging_dir.join(rel_path).exists() {
            continue;
        }
        match sha256_file(&app_dir.join(rel_path)) {
            Ok(hash) if hash == entry.sha256 => {}
            Ok(hash) => {
                return Err(format!(
                    "{rel_path} is absent from the archive and the local copy does not match \
                     the manifest: expected {}, found {hash}",
                    entry.sha256
                ));
            }
            Err(e) => {
                return Err(format!(
                    "{rel_path} is absent from the archive and unreadable locally: {e}"
                ));
            }
        }
    }
    Ok(())
}
