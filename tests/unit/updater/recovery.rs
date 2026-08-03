//! Tests for `src/updater/recovery.rs`, attached to it by `#[path]`.

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

    // is_new=true -> an unguarded rollback would remove app_dir/../victim.txt
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
