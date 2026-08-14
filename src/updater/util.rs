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
/// as "not newer"; we never offer or advance to an unparseable version).
pub(super) fn is_newer(remote: &str, current: &str) -> bool {
    match (dev_order_key(remote), dev_order_key(current)) {
        (Some(remote), Some(current)) => remote > current,
        _ => false,
    }
}

/// True if `installed` satisfies the manifest's minimum-version floor
/// (`installed >= min_version`), the skip-migration gate. Same dev-aware
/// ordering as `is_newer`: the two gates can never disagree.
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
/// install (it's a `.tar.gz` or a dev build); the cross-track gate does not
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
#[path = "../../tests/unit/updater/util/sandbox_protocol_gate_tests.rs"]
mod sandbox_protocol_gate_tests;

#[cfg(test)]
#[path = "../../tests/unit/updater/util/version_tests.rs"]
mod version_tests;
