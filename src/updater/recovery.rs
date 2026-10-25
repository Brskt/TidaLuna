use std::fs;
use std::path::Path;

use super::types::Journal;
use super::util::{exe_dir, is_safe_relative_path};

/// Proof that the install folder holds no half-applied update; only [`recover_interrupted_update`]
/// makes one. The front folder, install guard and Bun probe demand it: the updater swaps
/// `frontend/frontend.json` ahead of the exe, and a front check run first would refuse the state
/// the rollback repairs. It does not cover libcef (loaded earlier by the OS loader: Windows
/// delay-load, Linux RUNPATH) nor the Linux chrome-sandbox check; CEF `initialize` follows the
/// recovery by position alone.
pub(crate) struct Recovered(());

/// Check for and resolve any interrupted update journal.
/// Must be called early in main(), before CEF init.
pub(crate) fn recover_interrupted_update() -> Recovered {
    if let Some(app_dir) = exe_dir() {
        recover_in(&app_dir);
    }
    Recovered(())
}

/// Resolve the update journal in `app_dir`, if there is one.
fn recover_in(app_dir: &Path) {
    let journal_path = app_dir.join(".update-journal.json");
    if !journal_path.exists() {
        return;
    }

    crate::vprintln!("[UPDATER] Found update journal, recovering...");

    let data = match fs::read_to_string(&journal_path) {
        Ok(d) => d,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to read journal: {e}");
            let _ = fs::remove_file(&journal_path);
            return;
        }
    };

    let journal: Journal = match serde_json::from_str(&data) {
        Ok(j) => j,
        Err(e) => {
            crate::vprintln!("[UPDATER] Failed to parse journal: {e}");
            let _ = fs::remove_file(&journal_path);
            return;
        }
    };

    apply_journal(app_dir, &journal);

    // Clean up staging and journal
    let staging = app_dir.join(".update-staging");
    if staging.exists() {
        fs::remove_dir_all(&staging).ok();
    }
    fs::remove_file(&journal_path).ok();
    crate::vprintln!("[UPDATER] Recovery complete");
}

/// Apply the recovery actions described by `journal`, relative to `app_dir`.
fn apply_journal(app_dir: &Path, journal: &Journal) {
    match journal.state.as_str() {
        "pending" => {
            // Rollback: restore .bak -> original
            crate::vprintln!(
                "[UPDATER] Rolling back incomplete update v{}",
                journal.version
            );
            for jf in &journal.files {
                // The journal is unsigned on-disk state; never trust its paths to
                // stay inside app_dir on re-read (path-traversal guard).
                if !is_safe_relative_path(&jf.path, app_dir) {
                    continue;
                }
                let original = app_dir.join(&jf.path);
                if jf.is_new {
                    // No original existed - remove the newly installed file
                    fs::remove_file(&original).ok();
                } else if is_safe_relative_path(&jf.backup, app_dir) {
                    let backup = app_dir.join(&jf.backup);
                    if backup.exists() {
                        if original.exists() {
                            fs::remove_file(&original).ok();
                        }
                        fs::rename(&backup, &original).ok();
                    }
                }
            }
        }
        "committed" => {
            // Cleanup: remove .bak files + obsolete files from old layout
            crate::vprintln!(
                "[UPDATER] Cleaning up completed update v{}",
                journal.version
            );
            for jf in &journal.files {
                if !is_safe_relative_path(&jf.backup, app_dir) {
                    continue;
                }
                let backup = app_dir.join(&jf.backup);
                fs::remove_file(&backup).ok();
            }
            for del_path in &journal.deleted_files {
                if !is_safe_relative_path(del_path, app_dir) {
                    continue;
                }
                fs::remove_file(app_dir.join(del_path)).ok();
            }
        }
        other => {
            crate::vprintln!("[UPDATER] Unknown journal state: {other}");
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/updater/recovery.rs"]
mod tests;
