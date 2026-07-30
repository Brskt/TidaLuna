use std::fs;
use std::path::Path;

use super::types::Journal;
use super::util::{exe_dir, is_safe_relative_path};

/// Check for and resolve any interrupted update journal.
/// Must be called early in main(), before CEF init.
pub(crate) fn recover_interrupted_update() {
    let app_dir = match exe_dir() {
        Some(d) => d,
        None => return,
    };

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

    apply_journal(&app_dir, &journal);

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
            // Rollback: restore .bak → original
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
