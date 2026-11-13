use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio, exit};

/// Kinds cargo keeps as a prefix on each `linked_paths` entry.
const LINK_KINDS: [&str; 5] = ["native=", "dependency=", "crate=", "framework=", "all="];

/// Build the binary and return the CEF distribution cef-dll-sys linked it against.
/// The path is read from cargo's JSON messages: the build-dir layout is internal to
/// cargo and moved in 1.100, which left a glob over it with nothing to find.
pub(super) fn build_and_find_cef_dir(release: bool) -> Result<PathBuf, String> {
    // Only match the CEF library for the current target (avoids picking Linux dirs on Windows)
    let cef_marker = if cfg!(target_os = "windows") {
        "libcef.dll"
    } else if cfg!(target_os = "macos") {
        "Chromium Embedded Framework.framework"
    } else {
        "libcef.so"
    };

    let mut cmd = Command::new("cargo");
    cmd.args(["build", "--message-format=json-render-diagnostics"]);
    if release {
        cmd.arg("--release");
    }
    // Progress and diagnostics stay on the inherited stderr; stdout carries only JSON.
    let mut child = cmd
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to run cargo: {e}"))?;
    let stdout = child.stdout.take().ok_or("cargo stdout was not captured")?;

    // Read to the end before waiting, or a full pipe would stall cargo.
    let mut found: Vec<PathBuf> = Vec::new();
    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(|e| format!("cannot read cargo output: {e}"))?;
        for dir in linked_dirs(&line) {
            if dir.join(cef_marker).exists() && !found.contains(&dir) {
                found.push(dir);
            }
        }
    }

    let status = child
        .wait()
        .map_err(|e| format!("failed to wait for cargo: {e}"))?;
    if !status.success() {
        exit(status.code().unwrap_or(1));
    }

    match found.as_slice() {
        [dir] => Ok(dir.clone()),
        [] => Err(format!("no library path from the build holds {cef_marker}")),
        _ => Err(format!(
            "several library paths hold {cef_marker}: {found:?}"
        )),
    }
}

/// The directories a `build-script-executed` message adds to the library search path.
fn linked_dirs(line: &str) -> Vec<PathBuf> {
    let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    if msg["reason"] != "build-script-executed" {
        return Vec::new();
    }
    msg["linked_paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.as_str())
        .map(|entry| {
            let path = LINK_KINDS
                .iter()
                .find_map(|kind| entry.strip_prefix(kind))
                .unwrap_or(entry);
            PathBuf::from(path)
        })
        .collect()
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
