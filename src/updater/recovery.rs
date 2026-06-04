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
mod tests {
    use std::fs;

    use super::super::types::{Journal, JournalFile};
    use super::apply_journal;

    fn journal(state: &str, files: Vec<JournalFile>) -> Journal {
        Journal {
            version: "9.9.9-test".into(),
            state: state.into(),
            files,
            deleted_files: Vec::new(),
        }
    }

    fn jfile(path: &str, backup: &str, is_new: bool) -> JournalFile {
        JournalFile {
            path: path.into(),
            backup: backup.into(),
            is_new,
        }
    }

    // --- Path-traversal guards (the security fix) ---

    #[test]
    fn pending_rollback_skips_path_traversal_delete() {
        let root = tempfile::tempdir().unwrap();
        let app_dir = root.path().join("app");
        fs::create_dir(&app_dir).unwrap();
        let victim = root.path().join("victim.txt");
        fs::write(&victim, b"keep me").unwrap();

        // is_new=true → an unguarded rollback would remove app_dir/../victim.txt
        apply_journal(
            &app_dir,
            &journal("pending", vec![jfile("../victim.txt", "", true)]),
        );

        assert!(
            victim.exists(),
            "traversal path must not delete a file outside app_dir"
        );
    }

    #[test]
    fn pending_rollback_skips_backup_traversal_rename() {
        let root = tempfile::tempdir().unwrap();
        let app_dir = root.path().join("app");
        fs::create_dir(&app_dir).unwrap();
        let secret = root.path().join("secret.txt");
        fs::write(&secret, b"secret").unwrap();

        // path safe, backup escapes; unguarded code would rename it into app_dir
        apply_journal(
            &app_dir,
            &journal("pending", vec![jfile("app.bin", "../secret.txt", false)]),
        );

        assert!(secret.exists(), "escaping backup must stay in place");
        assert!(
            !app_dir.join("app.bin").exists(),
            "escaping backup must not be pulled into app_dir"
        );
    }

    #[test]
    fn committed_cleanup_skips_backup_traversal_delete() {
        let root = tempfile::tempdir().unwrap();
        let app_dir = root.path().join("app");
        fs::create_dir(&app_dir).unwrap();
        let victim = root.path().join("victim.txt");
        fs::write(&victim, b"keep me").unwrap();

        // unguarded committed cleanup would remove app_dir/../victim.txt
        apply_journal(
            &app_dir,
            &journal("committed", vec![jfile("app.bin", "../victim.txt", false)]),
        );

        assert!(
            victim.exists(),
            "traversal backup must not be deleted during cleanup"
        );
    }

    // --- Legitimate recovery still works ---

    #[test]
    fn pending_rollback_restores_safe_backup() {
        let root = tempfile::tempdir().unwrap();
        let app_dir = root.path().join("app");
        fs::create_dir(&app_dir).unwrap();
        fs::write(app_dir.join("app.bin"), b"new-broken").unwrap();
        fs::write(app_dir.join("app.bin.bak"), b"old-good").unwrap();

        apply_journal(
            &app_dir,
            &journal("pending", vec![jfile("app.bin", "app.bin.bak", false)]),
        );

        assert_eq!(fs::read(app_dir.join("app.bin")).unwrap(), b"old-good");
        assert!(!app_dir.join("app.bin.bak").exists());
    }

    #[test]
    fn pending_rollback_removes_new_file() {
        let root = tempfile::tempdir().unwrap();
        let app_dir = root.path().join("app");
        fs::create_dir(&app_dir).unwrap();
        fs::write(app_dir.join("added.bin"), b"brand-new").unwrap();

        apply_journal(
            &app_dir,
            &journal("pending", vec![jfile("added.bin", "", true)]),
        );

        assert!(!app_dir.join("added.bin").exists());
    }

    #[test]
    fn committed_cleanup_removes_safe_backup() {
        let root = tempfile::tempdir().unwrap();
        let app_dir = root.path().join("app");
        fs::create_dir(&app_dir).unwrap();
        fs::write(app_dir.join("app.bin.bak"), b"old").unwrap();

        apply_journal(
            &app_dir,
            &journal("committed", vec![jfile("app.bin", "app.bin.bak", false)]),
        );

        assert!(!app_dir.join("app.bin.bak").exists());
    }
}
