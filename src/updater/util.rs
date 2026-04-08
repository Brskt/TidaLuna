use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::types::{GhRelease, Manifest};

pub(super) fn extract_version_arg(msg: &crate::app_state::IpcMessage) -> String {
    msg.args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

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
