use std::fs;
use std::path::{Path, PathBuf};

/// Find the CEF distribution directory inside target/release/build/cef-dll-sys-*/out/
pub(super) fn find_cef_dir(target_dir: &Path) -> Result<PathBuf, String> {
    let build_dir = target_dir.join("build");
    let entries = fs::read_dir(&build_dir)
        .map_err(|e| format!("cannot read {}: {e}", build_dir.display()))?;

    // Only match the CEF library for the current target (avoids picking Linux dirs on Windows)
    let cef_marker = if cfg!(target_os = "windows") {
        "libcef.dll"
    } else if cfg!(target_os = "macos") {
        "Chromium Embedded Framework.framework"
    } else {
        "libcef.so"
    };

    let mut candidates: Vec<PathBuf> = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("cef-dll-sys-") {
            let out_dir = entry.path().join("out");
            if out_dir.is_dir() {
                for sub in fs::read_dir(&out_dir).into_iter().flatten().flatten() {
                    let sub_name = sub.file_name().to_string_lossy().to_string();
                    if sub_name.starts_with("cef_") && sub.path().is_dir() {
                        let marker_path = sub.path().join(cef_marker);
                        if marker_path.exists() || marker_path.is_dir() {
                            candidates.push(sub.path());
                        }
                    }
                }
            }
        }
    }

    candidates.sort_by(|a, b| {
        let mtime = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
        mtime(b).cmp(&mtime(a))
    });

    if let Some(best) = candidates.into_iter().next() {
        return Ok(best);
    }
    Err("CEF directory not found - run `cargo build --release` first".to_string())
}

/// Link or copy files from CEF dir to bundle (skip .exe, .lib, directories, build files).
/// Hard-link or copy a file, removing any existing destination first.
pub(super) fn link_or_copy(src: &Path, dst: &Path) -> Result<(), String> {
    let _ = fs::remove_file(dst);
    if fs::hard_link(src, dst).is_err() {
        fs::copy(src, dst).map_err(|e| format!("failed to copy {}: {e}", src.display()))?;
    }
    Ok(())
}

pub(super) fn copy_cef_files(cef_dir: &Path, bundle_dir: &Path) -> Result<(), String> {
    let skip_extensions = ["exe", "lib"];
    let skip_names = ["CMakeLists.txt", "cmake", "include", "libcef_dll"];

    let entries = fs::read_dir(cef_dir).map_err(|e| format!("cannot read CEF dir: {e}"))?;

    let mut count = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if path.is_dir() || skip_names.contains(&name.as_str()) {
            continue;
        }

        if let Some(ext) = path.extension()
            && skip_extensions.contains(&ext.to_string_lossy().as_ref())
        {
            continue;
        }

        let dst = bundle_dir.join(&name);
        link_or_copy(&path, &dst)?;
        count += 1;
    }
    println!("  Copied {count} CEF files");
    Ok(())
}

/// Link or copy all files from a directory (flat, no recursion).
pub(super) fn copy_dir_flat(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("failed to create {}: {e}", dst.display()))?;

    for entry in fs::read_dir(src).into_iter().flatten().flatten() {
        if entry.path().is_file() {
            let dest = dst.join(entry.file_name());
            link_or_copy(&entry.path(), &dest)?;
        }
    }
    Ok(())
}
