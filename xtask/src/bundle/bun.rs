use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{install_executable, project_root};

/// Where Bun goes: `bin/`, the first folder the app searches. In a macOS app it is
/// `Contents/MacOS` beside the exe, which Apple lists for helper tools; Bun stays out of the strip
/// pass there, keeping the signature Oven ships it with.
pub(super) fn bun_dir(bundle_dir: &Path, macos_app: Option<&str>) -> PathBuf {
    match macos_app {
        Some(exe) => bundle_dir.join(format!("{exe}.app/Contents/MacOS")),
        None => bundle_dir.join("bin"),
    }
}

/// Download Bun binary from GitHub releases into the bundle directory.
/// Uses a cache directory to avoid re-downloading on every build.
pub(super) fn download_bun(bundle_dir: &Path) -> Result<(), String> {
    let bun_name = if cfg!(target_os = "windows") {
        "bun.exe"
    } else {
        "bun"
    };
    let bun_dst = bundle_dir.join(bun_name);

    const BUN_VERSION: &str = "1.4.4";

    // Cache dir persists across builds (dist/ is cleaned each time)
    let cache_dir = project_root()?.join(".cache").join("bun");
    fs::create_dir_all(&cache_dir).map_err(|e| format!("failed to create cache dir: {e}"))?;

    let cached_bun = cache_dir.join(format!(
        "bun-v{BUN_VERSION}{}",
        if cfg!(target_os = "windows") {
            ".exe"
        } else {
            ""
        }
    ));

    // If cached binary exists for this version, just copy it
    if cached_bun.exists()
        && fs::metadata(&cached_bun)
            .map(|m| m.len() > 1_000_000)
            .unwrap_or(false)
    {
        install_executable(&cached_bun, &bun_dst)?;
        println!("  Bun v{BUN_VERSION} (cached)");
        return Ok(());
    }

    let (archive_name, inner_dir) = if cfg!(target_os = "windows") {
        ("bun-windows-x64.zip", "bun-windows-x64")
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            ("bun-darwin-aarch64.zip", "bun-darwin-aarch64")
        } else {
            ("bun-darwin-x64.zip", "bun-darwin-x64")
        }
    } else {
        ("bun-linux-x64.zip", "bun-linux-x64")
    };

    let url = format!(
        "https://github.com/oven-sh/bun/releases/download/bun-v{BUN_VERSION}/{archive_name}"
    );
    let zip_path = cache_dir.join(archive_name);

    println!("  Downloading Bun v{BUN_VERSION} from {url}...");
    let status = Command::new("curl")
        .args(["-fSL", "-o"])
        .arg(&zip_path)
        .arg(&url)
        .status()
        .map_err(|e| format!("curl failed: {e}"))?;
    if !status.success() {
        return Err(format!(
            "Failed to download Bun (curl exit: {:?})",
            status.code()
        ));
    }

    println!("  Extracting {bun_name}...");
    let extracted = extract_entry(&zip_path, &format!("{inner_dir}/{bun_name}"), &cached_bun);
    let _ = fs::remove_file(&zip_path);
    extracted?;

    install_executable(&cached_bun, &bun_dst)?;
    println!("  Bun v{BUN_VERSION} installed");
    Ok(())
}

/// Writes the archive's `entry` to `dst` through a `.part` file: an extraction cut short never
/// passes for the cached binary, which is taken on its size alone.
fn extract_entry(zip_path: &Path, entry: &str, dst: &Path) -> Result<(), String> {
    let file = fs::File::open(zip_path)
        .map_err(|e| format!("failed to open {}: {e}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| format!("failed to read {}: {e}", zip_path.display()))?;
    let mut source = archive
        .by_name(entry)
        .map_err(|e| format!("{entry} is not in {}: {e}", zip_path.display()))?;
    let mut part = dst.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let mut out =
        fs::File::create(&part).map_err(|e| format!("failed to create {}: {e}", part.display()))?;
    io::copy(&mut source, &mut out).map_err(|e| format!("failed to extract {entry}: {e}"))?;
    drop(out);
    fs::rename(&part, dst).map_err(|e| format!("failed to move {}: {e}", part.display()))
}

#[cfg(test)]
#[path = "../../tests/unit/bundle/bun/tests.rs"]
mod tests;
