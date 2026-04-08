use std::fs;

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

    match journal.state.as_str() {
        "pending" => {
            // Rollback: restore .bak → original
            crate::vprintln!(
                "[UPDATER] Rolling back incomplete update v{}",
                journal.version
            );
            for jf in &journal.files {
                let original = app_dir.join(&jf.path);
                let backup = app_dir.join(&jf.backup);
                if jf.is_new {
                    // No original existed — remove the newly installed file
                    fs::remove_file(&original).ok();
                } else if backup.exists() {
                    if original.exists() {
                        fs::remove_file(&original).ok();
                    }
                    fs::rename(&backup, &original).ok();
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
                let backup = app_dir.join(&jf.backup);
                fs::remove_file(&backup).ok();
            }
            for del_path in &journal.deleted_files {
                if !is_safe_relative_path(del_path, &app_dir) {
                    continue;
                }
                fs::remove_file(app_dir.join(del_path)).ok();
            }
        }
        other => {
            crate::vprintln!("[UPDATER] Unknown journal state: {other}");
        }
    }

    // Clean up staging and journal
    let staging = app_dir.join(".update-staging");
    if staging.exists() {
        fs::remove_dir_all(&staging).ok();
    }
    fs::remove_file(&journal_path).ok();
    crate::vprintln!("[UPDATER] Recovery complete");
}
