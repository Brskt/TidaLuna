use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{front_files, run_in};

/// Where `bun` lives: `TIDALUNAR_BUN`, then PATH, then `$BUN_INSTALL/bin` (default `~/.bun/bin`),
/// which an IDE running cargo without a shell rc file still finds.
fn resolve_bun() -> Result<PathBuf, String> {
    if let Some(explicit) = std::env::var_os("TIDALUNAR_BUN")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
    {
        return Ok(explicit);
    }
    if Command::new("bun").arg("--version").output().is_ok() {
        return Ok(PathBuf::from("bun"));
    }
    let candidate = std::env::var_os("BUN_INSTALL")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(|home| Path::new(&home).join(".bun"))
        })
        .map(|root| {
            root.join("bin")
                .join(if cfg!(windows) { "bun.exe" } else { "bun" })
        })
        .filter(|p| p.is_file());
    candidate.ok_or_else(|| {
        "bun not found: install it from https://bun.sh, set TIDALUNAR_BUN to its full path, \
         or pass --prebuilt-front to reuse frontend/dist"
            .to_string()
    })
}

pub(super) fn build_front(project_root: &Path) -> Result<(), String> {
    let front = project_root.join("frontend");
    let bun = resolve_bun()?;
    if !front.join("node_modules").exists() {
        println!("Installing front dependencies...");
        run_in(&bun, &["install"], &front)?;
    }
    println!("Building the front...");
    run_in(&bun, &["esbuild.config.ts"], &front)
}

/// Emptied first, then filled by allowlist. A missing file fails the bundle (the app would
/// refuse to start without it).
pub(super) fn copy_front(src: &Path, dst: &Path) -> Result<(), String> {
    if dst.exists() {
        fs::remove_dir_all(dst).map_err(|e| format!("failed to empty {}: {e}", dst.display()))?;
    }
    let missing = |path: &Path| {
        format!(
            "{} is missing: build the front first (bun esbuild.config.ts in frontend/)",
            path.display()
        )
    };
    let early_src = src.join("early-runtime");
    let mut files: Vec<PathBuf> = front_files::TOP_FILES.iter().map(PathBuf::from).collect();
    let entries = fs::read_dir(&early_src).map_err(|_| missing(&early_src))?;
    for entry in entries {
        let name = entry
            .map_err(|e| format!("failed to read {}: {e}", early_src.display()))?
            .file_name();
        if name.to_string_lossy().ends_with(".js") {
            files.push(Path::new("early-runtime").join(name));
        }
    }
    for rel in &files {
        let from = src.join(rel);
        if !from.is_file() {
            return Err(missing(&from));
        }
        let to = dst.join(rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        fs::copy(&from, &to).map_err(|e| format!("failed to copy {}: {e}", from.display()))?;
    }
    println!("  Copied the front into {}", dst.display());
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/main/tests.rs"]
mod tests;
