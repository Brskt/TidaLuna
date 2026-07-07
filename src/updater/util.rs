use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::types::{GhRelease, Manifest};

async fn fetch_gh_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    endpoint: &str,
) -> Result<T, anyhow::Error> {
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
    serde_json::from_str(&body).context("parse release")
}

pub(super) async fn fetch_gh_release(
    client: &reqwest::Client,
    endpoint: &str,
) -> Result<GhRelease, anyhow::Error> {
    fetch_gh_json(client, endpoint).await
}

/// The repo's release list, newest first (one page; the fork's whole history
/// fits well under the 100-item cap).
pub(super) async fn fetch_gh_releases(
    client: &reqwest::Client,
) -> Result<Vec<GhRelease>, anyhow::Error> {
    fetch_gh_json(client, "releases?per_page=100").await
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

/// Ordering key for release versions with CI dev builds in the mix: SemVer
/// ranks `X.Y.Z-alpha.dev.5` ABOVE `X.Y.Z-alpha` (a longer prerelease list
/// wins on a shared prefix), but a promoted release must outrank the dev
/// builds it was cut from.
pub(super) fn dev_order_key(v: &str) -> Option<(semver::Version, bool, u64)> {
    let parsed = semver::Version::parse(v).ok()?;
    let ids: Vec<&str> = parsed.pre.split('.').collect();
    let dev_n = match ids.as_slice() {
        [.., "dev", n] => n.parse::<u64>().ok(),
        _ => None,
    };
    let mut base = parsed.clone();
    if dev_n.is_some() {
        let base_pre = ids[..ids.len() - 2].join(".");
        base.pre = semver::Prerelease::new(&base_pre).unwrap_or_default();
    }
    // Key: base version, then final-beats-dev (true > false), then the counter.
    Some((base, dev_n.is_none(), dev_n.unwrap_or(0)))
}

/// True if `remote` is a strictly newer version than `current`, with dev
/// builds (`X.Y.Z-pre.dev.N`) ranking below their promoted release.
///
/// Fail-safe: if either string is not valid SemVer, returns `false` (treated
/// as "not newer", so we never offer or advance to an unparseable version).
pub(super) fn is_newer(remote: &str, current: &str) -> bool {
    match (dev_order_key(remote), dev_order_key(current)) {
        (Some(remote), Some(current)) => remote > current,
        _ => false,
    }
}

/// True if `installed` satisfies the manifest's minimum-version floor
/// (`installed >= min_version`), the skip-migration gate. Same dev-aware
/// ordering as `is_newer` so the two gates can never disagree.
///
/// Fail-closed: an unparseable floor or installed version blocks the update.
/// A no-op floor is encoded as `"0.0.0"` (every valid version satisfies it).
pub(super) fn meets_min_version(installed: &str, min_version: &str) -> bool {
    match (dev_order_key(installed), dev_order_key(min_version)) {
        (Some(installed), Some(floor)) => installed >= floor,
        _ => false,
    }
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

#[cfg(test)]
mod version_tests {
    use super::{is_newer, meets_min_version};

    #[test]
    fn is_newer_orders_patch_numerically() {
        assert!(is_newer("0.0.10-alpha", "0.0.9-alpha"));
        assert!(!is_newer("0.0.9-alpha", "0.0.10-alpha"));
    }

    #[test]
    fn is_newer_release_beats_its_prerelease() {
        // SemVer 2.0: 0.0.9 > 0.0.9-alpha
        assert!(is_newer("0.0.9", "0.0.9-alpha"));
        assert!(!is_newer("0.0.9-alpha", "0.0.9"));
    }

    #[test]
    fn is_newer_orders_prerelease_identifiers() {
        // -alpha.10 > -alpha.2 (numeric identifier compare, not string)
        assert!(is_newer("0.0.9-alpha.10", "0.0.9-alpha.2"));
        assert!(!is_newer("0.0.9-alpha.2", "0.0.9-alpha.10"));
    }

    #[test]
    fn is_newer_equal_is_not_newer() {
        assert!(!is_newer("0.0.9-alpha", "0.0.9-alpha"));
    }

    #[test]
    fn is_newer_unparseable_is_failsafe_false() {
        assert!(!is_newer("garbage", "0.0.9-alpha"));
        assert!(!is_newer("0.0.9-alpha", "not-a-version"));
    }

    #[test]
    fn meets_min_version_floor_satisfied() {
        assert!(meets_min_version("0.0.9-alpha", "0.0.0")); // no-op floor
        assert!(meets_min_version("0.0.9-alpha", "0.0.8-alpha"));
        assert!(meets_min_version("0.0.8-alpha", "0.0.8-alpha")); // equal meets floor
    }

    #[test]
    fn meets_min_version_below_floor_blocked() {
        assert!(!meets_min_version("0.0.7-alpha", "0.0.8-alpha"));
    }

    #[test]
    fn meets_min_version_unparseable_is_failclosed() {
        assert!(!meets_min_version("0.0.9-alpha", "")); // empty floor → block
        assert!(!meets_min_version("0.0.9-alpha", "garbage"));
    }

    #[test]
    fn promoted_release_beats_its_dev_builds() {
        // Raw SemVer says alpha.dev.5 > alpha; the dev-aware key inverts that.
        assert!(is_newer("0.0.14-alpha", "0.0.14-alpha.dev.5"));
        assert!(!is_newer("0.0.14-alpha.dev.5", "0.0.14-alpha"));
        // Same rule in the bare-release phase.
        assert!(is_newer("0.1.0", "0.1.0-dev.3"));
        assert!(!is_newer("0.1.0-dev.3", "0.1.0"));
    }

    #[test]
    fn dev_counters_compare_numerically() {
        assert!(is_newer("0.0.14-alpha.dev.10", "0.0.14-alpha.dev.9"));
        assert!(!is_newer("0.0.14-alpha.dev.9", "0.0.14-alpha.dev.10"));
    }

    #[test]
    fn dev_builds_of_a_newer_base_beat_older_releases() {
        assert!(is_newer("0.0.14-alpha.dev.1", "0.0.13-alpha"));
        assert!(is_newer("0.0.20-beta", "0.0.20-alpha.dev.7")); // phase change wins
        assert!(!is_newer("0.0.13-alpha", "0.0.14-alpha.dev.1"));
    }

    #[test]
    fn non_dev_prerelease_lists_keep_semver_order() {
        // A trailing identifier pair that is not `dev.N` stays raw SemVer.
        assert!(is_newer("0.0.9-alpha.rc.2", "0.0.9-alpha"));
    }

    #[test]
    fn meets_min_version_is_dev_aware() {
        // A dev build sits below its promoted base, so it misses that floor.
        assert!(!meets_min_version("0.0.14-alpha.dev.5", "0.0.14-alpha"));
        assert!(meets_min_version("0.0.14-alpha", "0.0.14-alpha.dev.5"));
    }
}
