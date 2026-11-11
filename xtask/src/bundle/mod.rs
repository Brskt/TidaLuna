use std::fs;
use std::path::Path;
use std::process::Command;

use crate::manifest::generate_manifest;
use crate::{project_root, run};

mod bun;
mod cef;
mod front;
mod macos;

use bun::{bun_dir, download_bun};
use cef::{copy_cef_files, copy_dir_flat, find_cef_dir, link_or_copy};
use front::{build_front, copy_front};
use macos::bundle_macos;

pub(crate) fn bundle(flags: &[String]) -> Result<(), String> {
    const EXE_NAME: &str = if cfg!(target_os = "windows") {
        "tidalunar.exe"
    } else {
        "tidalunar"
    };

    let release = flags.iter().any(|f| f == "--release");
    let prebuilt_front = flags.iter().any(|f| f == "--prebuilt-front");
    let profile = if release { "release" } else { "dev" };
    let project_root = project_root()?;

    // 1. Build the front, then the binary (neither needs the other).
    if !prebuilt_front {
        build_front(project_root)?;
    }
    println!("Building {profile}...");
    let mut args = vec!["build"];
    if release {
        args.push("--release");
    }
    run("cargo", &args)?;

    // 2. Locate target dir
    let target_dir = project_root
        .join("target")
        .join(if release { "release" } else { "debug" });
    let bundle_dir = project_root.join("dist");

    // 3. Find CEF directory from build output
    let cef_dir = find_cef_dir(&target_dir)?;
    println!("CEF dir: {}", cef_dir.display());

    // 4. Ensure bundle directory exists (incremental - no full wipe)
    fs::create_dir_all(&bundle_dir).map_err(|e| format!("failed to create dist/: {e}"))?;

    // 4b. Pre-clean: delete files from previous manifest that won't be in the new build.
    //     Runs BEFORE copying new files, keeping stale artifacts out of the output.
    {
        let old_manifest_path = bundle_dir.join("manifest.json");
        if old_manifest_path.exists() {
            let data = fs::read_to_string(&old_manifest_path).unwrap_or_default();
            if let Some(old_files) = serde_json::from_str::<serde_json::Value>(&data)
                .ok()
                .and_then(|v| v.get("files")?.as_object().cloned())
            {
                // Known files in the new layout (anything outside these paths is stale).
                // `bin/cef/` is absent here: it gets the stricter rule below.
                let new_layout_prefixes: &[&str] = if cfg!(target_os = "macos") {
                    &[] // macOS uses .app bundle, no flat layout
                } else {
                    &["bin/bun"]
                };
                let new_layout_roots: &[&str] = if cfg!(target_os = "macos") {
                    &[]
                } else {
                    &[
                        EXE_NAME,
                        "updater.exe",
                        "updater",
                        "manifest.json",
                        "archive.json",
                    ]
                };

                let mut cleaned = 0u32;
                const CEF_PREFIX: &str = "bin/cef/";
                for old_path in old_files.keys() {
                    // Everything under bin/cef/ comes from the CEF distribution; an entry
                    // whose source is gone is one it no longer ships, be it a renamed library
                    // or another platform's leftovers. The updater's manifest is built from
                    // whatever survives this loop.
                    let stale = match old_path.strip_prefix(CEF_PREFIX) {
                        Some(rest) if !cfg!(target_os = "macos") => !cef_dir.join(rest).exists(),
                        _ => {
                            !new_layout_roots.iter().any(|r| old_path == *r)
                                && !new_layout_prefixes.iter().any(|p| old_path.starts_with(p))
                        }
                    };
                    if stale && fs::remove_file(bundle_dir.join(old_path)).is_ok() {
                        cleaned += 1;
                    }
                }
                // Remove empty dirs left behind
                for old_path in old_files.keys() {
                    if let Some(parent) = Path::new(old_path).parent()
                        && !parent.as_os_str().is_empty()
                    {
                        let _ = fs::remove_dir(bundle_dir.join(parent));
                    }
                }
                if cleaned > 0 {
                    println!("  Pre-cleaned {cleaned} stale files from previous layout");
                }
            }
        }
    }

    // 5. Platform-specific bundling
    let bin_dir = bundle_dir.join("bin");
    if cfg!(target_os = "macos") {
        bundle_macos(EXE_NAME, project_root, &target_dir, &cef_dir, &bundle_dir)?;
    } else {
        // Linux/Windows: structured layout
        // Root: exe + updater + manifest
        let exe_src = target_dir.join(EXE_NAME);
        let exe_dst = bundle_dir.join(EXE_NAME);
        link_or_copy(&exe_src, &exe_dst)?;
        println!("  Copied {EXE_NAME}");

        // bin/cef/: all CEF runtime files
        let cef_bundle_dir = bin_dir.join("cef");
        fs::create_dir_all(&cef_bundle_dir)
            .map_err(|e| format!("failed to create bin/cef/: {e}"))?;

        copy_cef_files(&cef_dir, &cef_bundle_dir)?;

        let locales_src = cef_dir.join("locales");
        let locales_dst = cef_bundle_dir.join("locales");
        if locales_src.is_dir() {
            copy_dir_flat(&locales_src, &locales_dst)?;
            println!("  Copied bin/cef/locales/");
        }
    }

    // 5b. The front folder, read by the app at launch.
    let front_dst = if cfg!(target_os = "macos") {
        bundle_dir.join(format!("{EXE_NAME}.app/Contents/Resources/frontend"))
    } else {
        bundle_dir.join("frontend")
    };
    copy_front(&project_root.join("frontend/dist"), &front_dst)?;

    // 6. Download Bun binary
    let bun_dst = bun_dir(&bundle_dir, cfg!(target_os = "macos").then_some(EXE_NAME));
    fs::create_dir_all(&bun_dst)
        .map_err(|e| format!("failed to create {}: {e}", bun_dst.display()))?;
    download_bun(&bun_dst)?;

    if release {
        strip_binaries(&bundle_dir)?;
    }

    // 8. Generate manifest.json
    generate_manifest(&bundle_dir)?;

    println!("Bundle created in: {}", bundle_dir.display());
    Ok(())
}

pub(crate) fn build_updater(flags: &[String]) -> Result<(), String> {
    let release = flags.iter().any(|f| f == "--release");

    let mut args = vec!["build", "--package", "updater"];
    if release {
        args.push("--release");
    }
    run("cargo", &args)?;

    let project_root = project_root()?;
    let target_dir = project_root
        .join("target")
        .join(if release { "release" } else { "debug" });
    let bundle_dir = project_root.join("dist");

    let updater_name = if cfg!(target_os = "windows") {
        "updater.exe"
    } else {
        "updater"
    };

    let src = target_dir.join(updater_name);
    let dst = bundle_dir.join(updater_name);
    fs::copy(&src, &dst).map_err(|e| format!("copy updater binary: {e}"))?;

    println!("Updater binary copied to {}", dst.display());

    // Regenerate manifest to include the updater binary
    generate_manifest(&bundle_dir)?;

    Ok(())
}

fn strip_binaries(bundle_dir: &Path) -> Result<(), String> {
    if cfg!(target_os = "windows") {
        return Ok(());
    }

    let strip_args: &[&str] = if cfg!(target_os = "macos") {
        &["-x"]
    } else {
        &["--strip-debug", "--strip-unneeded"]
    };

    let should_strip = |name: &str| -> bool {
        name.ends_with(".so") || name.contains(".so.") || name.ends_with(".dylib") || name == "bun"
    };

    let mut stripped = 0u32;
    // Scan root + bin/ for strippable binaries
    let dirs_to_scan = [bundle_dir.to_path_buf(), bundle_dir.join("bin")];
    for scan_dir in &dirs_to_scan {
        for entry in fs::read_dir(scan_dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if path.is_file() && should_strip(&name) {
                let size_before = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                if run_strip(&path, strip_args)? {
                    let size_after = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    let saved_mb = (size_before.saturating_sub(size_after)) as f64 / 1_048_576.0;
                    println!("  Stripped {name} ({saved_mb:.1} MB saved)");
                    stripped += 1;
                } else {
                    println!("  Warning: strip failed for {name}");
                }
            }
        }
    }

    if cfg!(target_os = "macos") {
        strip_macos_app(bundle_dir, strip_args)?;
    }

    if stripped > 0 {
        println!("  Stripped {stripped} binaries");
    }
    Ok(())
}

fn run_strip(path: &Path, strip_args: &[&str]) -> Result<bool, String> {
    let status = Command::new("strip")
        .args(strip_args)
        .arg(path)
        .status()
        .map_err(|e| format!("strip failed for {}: {e}", path.display()))?;
    Ok(status.success())
}

fn strip_macos_app(bundle_dir: &Path, strip_args: &[&str]) -> Result<(), String> {
    fn walk_strip(dir: &Path, strip_args: &[&str]) -> Result<(), String> {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_strip(&path, strip_args)?;
            } else if path.is_file() {
                let name = entry.file_name().to_string_lossy().to_string();
                if (name.ends_with(".dylib") || name == "Chromium Embedded Framework")
                    && run_strip(&path, strip_args)?
                {
                    println!("  Stripped {name}");
                }
            }
        }
        Ok(())
    }
    walk_strip(bundle_dir, strip_args)
}
