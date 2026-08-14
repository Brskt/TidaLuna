//! Manifest-diff cleanup: compute and delete files present in an old manifest
//! but absent from the new one, gated by the existing path-safety guard.

use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::Manifest;
use crate::is_safe_relative_path;

/// Read and parse a manifest from disk. Returns `None` for missing or
/// unparseable input: callers treat that as "no diff input" rather
/// than a hard error, matching the apply-path's existing behavior on a
/// missing or corrupt installed manifest.
pub(crate) fn read_manifest(path: &Path) -> Option<Manifest> {
    let data = fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Paths present in `old` but absent from `new`, filtered through
/// `is_safe_relative_path`; absolute / `..` / drive-prefix entries cannot
/// escape `app_dir`.
pub(crate) fn diff_removed(old: &Manifest, new: &Manifest, app_dir: &Path) -> Vec<String> {
    old.files
        .keys()
        .filter(|p| !new.files.contains_key(p.as_str()) && is_safe_relative_path(p, app_dir))
        .cloned()
        .collect()
}

/// Delete each path under `app_dir`. `NotFound` is benign (already gone) and
/// counted as a no-op success; every other `io::Error` is propagated for the
/// caller to decide whether to fail.
///
/// The installer's transactional retry of `manifest.old.json` depends on this
/// helper actually exiting non-zero when a file could not be removed (e.g.
/// AV-locked, ACL-denied) - silently swallowing those errors would let the
/// installer drop the only diff input on the next install attempt.
fn delete_paths(to_remove: &[String], app_dir: &Path) -> Result<usize> {
    let mut removed = 0usize;
    for rel in to_remove {
        let target = app_dir.join(rel);
        match fs::remove_file(&target) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("remove stale file {rel}"));
            }
        }
    }
    Ok(removed)
}

/// One-shot cleanup driver used by `--cleanup-stale`. Reads both manifests,
/// computes the diff, deletes. Hard errors on `app_dir` not being a
/// directory, `new_manifest` being unparseable, or any per-file delete
/// failure other than `NotFound`. Treats a missing or unreadable old
/// manifest as "no work" (returns Ok(0)), letting first installs no-op cleanly.
pub(crate) fn cleanup_stale(
    app_dir: &Path,
    old_manifest: &Path,
    new_manifest: &Path,
) -> Result<usize> {
    if !app_dir.is_dir() {
        bail!("--app-dir not a directory: {}", app_dir.display());
    }
    let Some(old) = read_manifest(old_manifest) else {
        return Ok(0);
    };
    let new = read_manifest(new_manifest).context(format!(
        "could not parse --new-manifest: {}",
        new_manifest.display()
    ))?;
    let removed = diff_removed(&old, &new, app_dir);
    delete_paths(&removed, app_dir)
}
