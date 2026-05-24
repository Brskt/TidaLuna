use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::types::{GhRelease, Manifest};

pub(super) async fn fetch_gh_release(
    client: &reqwest::Client,
    endpoint: &str,
) -> Result<GhRelease, anyhow::Error> {
    use anyhow::Context;

    let current_version = env!("CARGO_PKG_VERSION");
    let url = format!(
        "https://api.github.com/repos/{}/{}/{}",
        super::GITHUB_OWNER,
        super::GITHUB_REPO,
        endpoint
    );

    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", format!("TidaLunar/{current_version}"))
        .send()
        .await
        .context("fetch release")?;

    if !resp.status().is_success() {
        anyhow::bail!("GitHub API returned {}", resp.status());
    }

    let body = resp.text().await.context("read release body")?;
    let release: GhRelease = serde_json::from_str(&body).context("parse release")?;
    Ok(release)
}

/// Returns the staged version if a valid pre-downloaded update exists.
pub(super) fn detect_staged_update() -> Option<String> {
    let app_dir = exe_dir()?;
    let manifest_name = super::manifest_name();
    let manifest_path = app_dir.join(".update-staging").join(&manifest_name);
    let data = fs::read_to_string(&manifest_path).ok()?;
    let manifest: Manifest = serde_json::from_str(&data).ok()?;
    if manifest.verify_target().is_err() {
        return None;
    }
    #[cfg(target_os = "linux")]
    if check_sandbox_protocol(&manifest, read_system_sandbox_protocol()).is_err() {
        // Staged update is incompatible with the current system bootstrap.
        // Don't surface it; the user must run apt upgrade first.
        return None;
    }
    let current = env!("CARGO_PKG_VERSION");
    if is_newer(&manifest.version, current) {
        Some(manifest.version)
    } else {
        None
    }
}

/// Reject absolute paths and directory-escape components.
pub(super) fn is_safe_relative_path(rel: &str, base: &Path) -> bool {
    let p = Path::new(rel);
    if p.is_absolute() {
        return false;
    }
    for c in p.components() {
        if matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::Prefix(_)
        ) {
            return false;
        }
    }
    base.join(rel).starts_with(base)
}

pub(super) fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

pub(super) fn sha256_file(path: &Path) -> Result<String, std::io::Error> {
    let mut file = fs::File::open(path)?;
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

/// Simple semver comparison: returns true if `remote` > `current`.
/// Strips pre-release suffixes (e.g. "-alpha") before comparing numeric parts.
pub(super) fn is_newer(remote: &str, current: &str) -> bool {
    let parse = |s: &str| -> (u32, u32, u32) {
        // Strip pre-release suffix: "0.0.2-alpha" → "0.0.2"
        let numeric = s.split('-').next().unwrap_or(s);
        let mut parts = numeric.split('.');
        let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        (major, minor, patch)
    };
    parse(remote) > parse(current)
}

// ---------------------------------------------------------------------------
// Sandbox protocol gate (Linux .deb cross-track compatibility)
// ---------------------------------------------------------------------------

/// Path to the system bootstrap's protocol-version file, populated by the
/// .deb's data.tar at install time. Track 2 (`apt upgrade tidalunar`) bumps
/// this; Track 1 (in-app updater) reads it.
#[cfg(target_os = "linux")]
const SYSTEM_PROTOCOL_PATH: &str = "/usr/lib/tidalunar/SANDBOX_PROTOCOL_VERSION";

/// Read the system bootstrap's protocol-version.
///
/// `None` means the file is absent/unreadable: this is NOT a packaged `.deb`
/// install (it's a `.tar.gz` or a dev build), so the cross-track gate does not
/// apply. `Some(0)` is the conservative value for a present-but-malformed file
/// on a real `.deb`. The gate only blocks when a system value is present and
/// lower than the manifest requires.
#[cfg(target_os = "linux")]
pub(super) fn read_system_sandbox_protocol() -> Option<u32> {
    read_system_sandbox_protocol_from(SYSTEM_PROTOCOL_PATH)
}

#[cfg(target_os = "linux")]
pub(super) fn read_system_sandbox_protocol_from(path: &str) -> Option<u32> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().parse::<u32>().unwrap_or(0))
}

/// Compare a manifest's required protocol against the system's. Returns Ok if
/// the in-app updater can safely apply this manifest's payload, or Err with a
/// user-facing diagnostic if the system bootstrap is too old. A `None` system
/// value (no `.deb` bootstrap present) skips the gate entirely.
#[cfg(target_os = "linux")]
pub(super) fn check_sandbox_protocol(
    manifest: &super::types::Manifest,
    system_protocol: Option<u32>,
) -> Result<(), anyhow::Error> {
    let Some(system_protocol) = system_protocol else {
        return Ok(());
    };
    let required = manifest.sandbox_protocol_required.unwrap_or(0);
    if required > system_protocol {
        anyhow::bail!(
            "Update v{} requires sandbox helper protocol {}, but system has {}. \
             Run 'sudo apt upgrade tidalunar' (or download a newer .deb from \
             https://github.com/Brskt/TidaLuna-lunar/releases) and try again.",
            manifest.version,
            required,
            system_protocol,
        );
    }
    Ok(())
}

/// Convenience wrapper that reads the system file fresh (no caching - apt
/// upgrade mid-session can change it) and applies the gate.
#[cfg(target_os = "linux")]
pub(super) fn enforce_sandbox_protocol_gate(
    manifest: &super::types::Manifest,
) -> Result<(), anyhow::Error> {
    let system = read_system_sandbox_protocol();
    check_sandbox_protocol(manifest, system)
}

#[cfg(all(test, target_os = "linux"))]
mod sandbox_protocol_gate_tests {
    use super::super::types::Manifest;
    use super::*;
    use std::collections::BTreeMap;

    fn fixture_manifest(required: Option<u32>) -> Manifest {
        Manifest {
            version: "0.0.5-alpha".into(),
            min_version: "0.0.4-alpha".into(),
            target: "linux-amd64".into(),
            files: BTreeMap::new(),
            sandbox_protocol_required: required,
            delta_from: None,
        }
    }

    #[test]
    fn read_system_protocol_missing_file_returns_none() {
        let v = read_system_sandbox_protocol_from("/nonexistent/path/SANDBOX_PROTOCOL_VERSION");
        assert_eq!(v, None);
    }

    #[test]
    fn read_system_protocol_parses_integer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SANDBOX_PROTOCOL_VERSION");
        std::fs::write(&path, "5\n").unwrap();
        let v = read_system_sandbox_protocol_from(path.to_str().unwrap());
        assert_eq!(v, Some(5));
    }

    #[test]
    fn read_system_protocol_corrupted_file_returns_some_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SANDBOX_PROTOCOL_VERSION");
        std::fs::write(&path, "not-a-number\n").unwrap();
        let v = read_system_sandbox_protocol_from(path.to_str().unwrap());
        assert_eq!(v, Some(0));
    }

    #[test]
    fn gate_passes_when_required_le_system() {
        let manifest = fixture_manifest(Some(1));
        let result = check_sandbox_protocol(&manifest, Some(1));
        assert!(result.is_ok());
    }

    #[test]
    fn gate_fails_when_required_gt_system() {
        let manifest = fixture_manifest(Some(2));
        let err = check_sandbox_protocol(&manifest, Some(1)).unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("requires sandbox helper protocol 2"), "got: {s}");
        assert!(s.contains("system has 1"), "got: {s}");
    }

    #[test]
    fn gate_passes_when_field_absent() {
        let manifest = fixture_manifest(None);
        let result = check_sandbox_protocol(&manifest, Some(0));
        assert!(result.is_ok());
    }

    #[test]
    fn gate_skipped_when_no_system_file() {
        // tar.gz / dev install: no system protocol file -> gate does not apply.
        let manifest = fixture_manifest(Some(2));
        let result = check_sandbox_protocol(&manifest, None);
        assert!(result.is_ok());
    }
}
