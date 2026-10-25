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

/// The journal is unsigned on-disk state: a path or backup climbing out of the app dir must
/// neither delete nor rename the file it reaches, in rollback and in cleanup. One row per guard.
#[test]
fn a_journal_entry_that_leaves_the_app_dir_touches_nothing_outside_it() {
    for (guard, state, entry) in [
        (
            "the rollback delete of a new file",
            "pending",
            jfile("../outside.txt", "", true),
        ),
        (
            "the rollback rename of a backup",
            "pending",
            jfile("app.bin", "../outside.txt", false),
        ),
        (
            "the cleanup delete of a backup",
            "committed",
            jfile("app.bin", "../outside.txt", false),
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let app_dir = root.path().join("app");
        fs::create_dir(&app_dir).unwrap();
        let outside = root.path().join("outside.txt");
        fs::write(&outside, b"keep me").unwrap();

        apply_journal(&app_dir, &journal(state, vec![entry]));

        assert!(outside.exists(), "{guard} reached a file outside app_dir");
        assert!(
            !app_dir.join("app.bin").exists(),
            "{guard} pulled a file into app_dir"
        );
    }
}

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
