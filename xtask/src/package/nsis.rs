use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::manifest::Manifest;
use crate::{numeric_version, project_root, read_workspace_version, run_status};

/// Sum the byte size of every regular file under `dir`, recursively.
/// Used for the installer's manual `AddSize` estimate: NSIS can't see inside
/// the pre-compressed payload.7z and needs the decompressed total.
fn dir_size_bytes(dir: &Path) -> Result<u64, String> {
    let mut total = 0u64;
    for entry in fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let ft = entry.file_type().map_err(|e| e.to_string())?;
        if ft.is_dir() {
            total += dir_size_bytes(&entry.path())?;
        } else if ft.is_file() {
            total += entry.metadata().map_err(|e| e.to_string())?.len();
        }
    }
    Ok(total)
}

pub(super) fn package_windows_nsis(arch: &str) -> Result<(), String> {
    let project_root = project_root()?;
    let dist = project_root.join("dist");
    if !dist.is_dir()
        || fs::read_dir(&dist)
            .map_err(|e| e.to_string())?
            .next()
            .is_none()
    {
        return Err("dist/ is empty or missing - run `cargo xtask bundle` first".into());
    }
    if !dist.join("manifest.json").is_file() {
        return Err("dist/manifest.json missing - run `cargo xtask bundle` first".into());
    }
    if !dist.join("updater.exe").is_file() {
        return Err("dist/updater.exe missing - run `cargo xtask build-updater` first".into());
    }

    // Validate dist payload arch matches --arch.
    let manifest_data =
        fs::read_to_string(dist.join("manifest.json")).map_err(|e| e.to_string())?;
    let manifest: Manifest = serde_json::from_str(&manifest_data).map_err(|e| e.to_string())?;
    let expected = format!("windows-{arch}");
    if manifest.target != expected {
        return Err(format!(
            "dist/ payload target {} does not match --arch {} (expected {})",
            manifest.target, arch, expected
        ));
    }

    // Prefer makensis on PATH, but fall back to the standard NSIS install
    // location on Windows: `choco install nsis` drops makensis.exe under
    // Program Files without reliably adding it to PATH.
    fn resolve_makensis() -> Option<PathBuf> {
        if Command::new("makensis").arg("/VERSION").output().is_ok() {
            return Some(PathBuf::from("makensis"));
        }
        #[cfg(windows)]
        {
            for var in ["ProgramFiles(x86)", "ProgramW6432", "ProgramFiles"] {
                if let Some(base) = std::env::var_os(var) {
                    let candidate = Path::new(&base).join("NSIS").join("makensis.exe");
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
        None
    }
    let makensis = resolve_makensis().ok_or_else(|| {
        "makensis not found - install via `apt install nsis` (Linux) or `choco install nsis` (Windows)"
            .to_string()
    })?;

    let out_dir = project_root.join("target").join("installer");
    fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;

    // Windows arch token for filenames (x64 instead of Debian's amd64).
    let win_arch = if arch == "amd64" { "x64" } else { "arm64" };

    // Remove any prior installers for this arch: older builds (typically a
    // version-suffixed .exe restored from cache or left from a local version
    // bump) cannot then be picked up by downstream globbers in CI (signtool,
    // Get-ChildItem | Select-Object -First 1, upload-artifact path glob).
    let stale_suffix = format!("_{win_arch}.exe");
    for entry in fs::read_dir(&out_dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("tidalunar_") && name_str.ends_with(&stale_suffix) {
            fs::remove_file(entry.path())
                .map_err(|e| format!("remove stale installer {name_str}: {e}"))?;
        }
    }

    let version = read_workspace_version()?;
    // NSIS VIProductVersion requires X.X.X.X with all numeric components, so
    // strip any semver prerelease/build suffix and pad to 4 parts. The
    // human-readable version still ships in DisplayVersion + filename.
    let version_numeric = numeric_version(&version, 4);
    let nsi = project_root
        .join("installer")
        .join("windows")
        .join("tidalunar.nsi");

    // Pre-compress the payload OUTSIDE makensis. makensis' builtin LZMA is
    // single-threaded and uses an outdated codec with no executable filter; on the
    // CEF payload it came out both larger and slower than 7-Zip's multithreaded
    // LZMA2. We build the solid .7z here and have the installer extract it at run
    // time via the bundled official 7zr.exe (see tidalunar.nsi).
    fn resolve_7z() -> Option<PathBuf> {
        for cand in ["7z", "7za"] {
            if Command::new(cand).output().is_ok() {
                return Some(PathBuf::from(cand));
            }
        }
        #[cfg(windows)]
        {
            for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
                if let Some(base) = std::env::var_os(var) {
                    let candidate = Path::new(&base).join("7-Zip").join("7z.exe");
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
        None
    }
    let sevenzip =
        resolve_7z().ok_or("7z/7za not found - install p7zip-full (Linux) or 7-Zip (Windows)")?;

    // Official 7-Zip standalone extractor shipped INSIDE the installer to
    // decompress payload.7z on the user's machine. Sourced via env (CI
    // downloads it and pins its SHA-256); no binary blob lives in git.
    let sevenzr = std::env::var_os("TIDALUNAR_7ZR_EXE")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .ok_or(
            "7zr.exe not found - set TIDALUNAR_7ZR_EXE to the official 7-Zip standalone \
             extractor (CI downloads it; locally point it at a trusted 7zr.exe)",
        )?;

    // Build the solid, max-compression payload archive. cwd = dist: entries
    // store at archive root (no `dist/` prefix), matching extraction straight
    // into $INSTDIR. -mmt=on uses all runner cores.
    let payload = out_dir.join("payload.7z");
    let _ = fs::remove_file(&payload);
    println!("Compressing payload with 7-Zip (LZMA2, solid, multithreaded)...");
    run_status(
        Command::new(&sevenzip)
            .current_dir(&dist)
            .args(["a", "-t7z", "-mx=9", "-ms=on", "-mmt=on"])
            .arg(&payload)
            .arg("."),
        "7z",
    )?;
    if !payload.is_file() {
        return Err("7z reported success but payload.7z is missing".into());
    }

    // AddSize wants the decompressed total in KB (NSIS can't see inside the .7z).
    let payload_kb = dir_size_bytes(&dist)?.div_ceil(1024);

    println!(
        "Packaging installer: target={expected} version={version} (numeric={version_numeric})"
    );
    run_status(
        Command::new(&makensis)
            .arg(format!("-DVERSION={version}"))
            .arg(format!("-DVERSION_NUMERIC={version_numeric}"))
            .arg(format!("-DARCH={win_arch}"))
            .arg(format!("-DPAYLOAD_7Z={}", payload.display()))
            .arg(format!("-DSEVENZR_EXE={}", sevenzr.display()))
            .arg(format!("-DPAYLOAD_KB={payload_kb}"))
            .arg(format!("-DOUT_DIR={}", out_dir.display()))
            .arg(&nsi),
        "makensis",
    )?;

    let exe = out_dir.join(format!("tidalunar_{version}_{win_arch}.exe"));
    if !exe.is_file() {
        return Err(format!(
            "expected installer at {} but file is missing",
            exe.display()
        ));
    }
    println!("Installer created: {}", exe.display());
    Ok(())
}
