use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{project_root, read_workspace_version};

/// Minimum `SANDBOX_PROTOCOL_VERSION` the Linux .deb's system bootstrap must
/// have for in-app updates produced by this xtask to be safe to apply.
///
/// Bump this value when CEF's major version changes or libcef changes the
/// SUID-sandbox helper protocol. The corresponding
/// `/usr/lib/tidalunar/SANDBOX_PROTOCOL_VERSION` file is written from this
/// constant by the .deb packaging pipeline, and the in-app updater compares
/// the value here (read via the manifest field) against the system file.
pub(crate) const LINUX_SANDBOX_PROTOCOL_REQUIRED: u32 = 1;

#[derive(Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub(crate) version: String,
    pub(crate) min_version: String,
    pub(crate) target: String,
    pub(crate) files: BTreeMap<String, FileEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sandbox_protocol_required: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) delta_from: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct FileEntry {
    pub(crate) sha256: String,
    pub(crate) size: u64,
}

fn target_triple() -> String {
    let os = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };

    let arch = if cfg!(target_arch = "x86_64") {
        "amd64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "unknown"
    };

    format!("{os}-{arch}")
}

fn sha256_file(path: &Path) -> Result<(String, u64), String> {
    let mut file =
        fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
    let size = metadata.len();

    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read error {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let hash = base16ct::lower::encode_string(&hasher.finalize());
    Ok((hash, size))
}

fn collect_files(
    dir: &Path,
    base: &Path,
    files: &mut BTreeMap<String, FileEntry>,
) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .strip_prefix(base)
            .map_err(|e| format!("strip prefix: {e}"))?
            .to_string_lossy()
            .replace('\\', "/");

        if path.is_dir() {
            collect_files(&path, base, files)?;
        } else if name != "manifest.json" && name != "manifest.json.sig" {
            let (sha256, size) = sha256_file(&path)?;
            files.insert(name, FileEntry { sha256, size });
        }
    }
    Ok(())
}

pub(crate) fn generate_manifest(bundle_dir: &Path) -> Result<(), String> {
    let version = read_workspace_version()?;

    // Upgrade floor: minimum installed version allowed to apply this update,
    // enforced in-app. "0.0.0" = no floor; keep it a concrete string (older
    // clients still parse the field).
    let min_version = "0.0.0".to_string();

    let target = target_triple();

    // Linux: declare the sandbox-helper protocol this payload requires. The
    // .deb launcher reads it from $USER_DIR/SANDBOX_PROTOCOL_REQUIRED to gate
    // against a stale system bootstrap. Written before collect_files: it
    // ships in the manifest, the update archives, and the payload tarball, and
    // is re-applied by in-app updates.
    if target.starts_with("linux-") {
        fs::write(
            bundle_dir.join("SANDBOX_PROTOCOL_REQUIRED"),
            format!("{LINUX_SANDBOX_PROTOCOL_REQUIRED}\n"),
        )
        .map_err(|e| format!("write SANDBOX_PROTOCOL_REQUIRED: {e}"))?;
    }

    let mut files = BTreeMap::new();
    collect_files(bundle_dir, bundle_dir, &mut files)?;

    let sandbox_protocol_required = if target.starts_with("linux-") {
        Some(LINUX_SANDBOX_PROTOCOL_REQUIRED)
    } else {
        None
    };
    let manifest = Manifest {
        version,
        min_version,
        target,
        files,
        sandbox_protocol_required,
        // CI stamps the real previous version into the published manifest.
        delta_from: None,
    };

    let json =
        serde_json::to_string_pretty(&manifest).map_err(|e| format!("serialize manifest: {e}"))?;
    fs::write(bundle_dir.join("manifest.json"), &json)
        .map_err(|e| format!("write manifest.json: {e}"))?;

    println!("  Generated manifest.json ({} files)", manifest.files.len());
    Ok(())
}

pub(crate) fn generate_keypair() -> Result<(), String> {
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).map_err(|e| format!("getrandom failed: {e}"))?;
    let signing_key = SigningKey::from_bytes(&secret);
    let verifying_key = VerifyingKey::from(&signing_key);

    let private_b64 = BASE64.encode(signing_key.to_bytes());
    let public_bytes = verifying_key.to_bytes();

    println!("=== Ed25519 Update Signing Keypair ===");
    println!();
    println!("PRIVATE KEY (store in GitHub Secret UPDATE_SIGNING_KEY):");
    println!("{private_b64}");
    println!();
    println!("PUBLIC KEY (embed in updater binary):");
    println!("const UPDATE_PUBLIC_KEY: [u8; 32] = {public_bytes:?};");
    println!();
    println!("PUBLIC KEY (base64):");
    println!("{}", BASE64.encode(public_bytes));

    Ok(())
}

pub(crate) fn sign_manifest() -> Result<(), String> {
    let key_b64 = std::env::var("UPDATE_SIGNING_KEY")
        .map_err(|_| "UPDATE_SIGNING_KEY environment variable not set".to_string())?;

    let key_bytes = BASE64
        .decode(key_b64.trim())
        .map_err(|e| format!("invalid base64 in UPDATE_SIGNING_KEY: {e}"))?;

    let key_array: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "UPDATE_SIGNING_KEY must be exactly 32 bytes (base64-encoded)".to_string())?;

    let signing_key = SigningKey::from_bytes(&key_array);

    let project_root = project_root()?;
    let manifest_path = project_root.join("dist/manifest.json");
    let sig_path = project_root.join("dist/manifest.json.sig");

    let manifest_bytes = fs::read(&manifest_path)
        .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))?;

    let signature = signing_key.sign(&manifest_bytes);
    let sig_b64 = BASE64.encode(signature.to_bytes());

    fs::write(&sig_path, &sig_b64)
        .map_err(|e| format!("cannot write {}: {e}", sig_path.display()))?;

    println!("Signed manifest.json -> manifest.json.sig");
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/manifest/manifest_emission_tests.rs"]
mod manifest_emission_tests;
