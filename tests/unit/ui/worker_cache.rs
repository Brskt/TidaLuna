//! Tests for `src/ui/worker_cache.rs`, attached to it by `#[path]`.
#![cfg(unix)]

use std::path::PathBuf;

use super::*;

const STORES: [&str; 3] = ["CacheStorage", "Database", "ScriptCache"];

fn leveldb(profile: &Path) -> PathBuf {
    profile.join("Local Storage").join("leveldb")
}

fn worker_with_stores(profile: &Path) {
    for store in STORES {
        let dir = profile.join(WORKER_DIR).join(store);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("entry"), b"stale").unwrap();
    }
}

fn worker_intact(profile: &Path) -> bool {
    STORES
        .iter()
        .all(|store| profile.join(WORKER_DIR).join(store).join("entry").exists())
}

fn recorded_epoch(profile: &Path) -> Option<Vec<u8>> {
    match sdk_storage::read_ls_value(&leveldb(profile), EPOCH_KEY) {
        LsValue::Value(bytes) => Some(bytes),
        _ => None,
    }
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A worker left in place leaves the new epoch unrecorded: recording it would make Rust and the
/// page's heal read the stale cache as current.
#[test]
fn a_worker_that_cannot_be_retired_leaves_the_epoch_unrecorded() {
    let dir = tempfile::tempdir().unwrap();
    worker_with_stores(dir.path());
    let profile = dir.path().to_path_buf();
    // Only the rename may fail: the LevelDB already exists and stays writable.
    let stale = b"fedcba9876543210";
    sdk_storage::write_ls_values(&leveldb(&profile), &[(EPOCH_KEY, stale)]).unwrap();
    set_mode(&profile, 0o555);
    assert!(
        std::fs::File::create(profile.join("probe")).is_err(),
        "the profile must refuse a write for this test to mean anything (not as root)"
    );

    drop_if_stale(&profile);
    set_mode(&profile, 0o755);

    assert!(worker_intact(&profile));
    assert_eq!(recorded_epoch(&profile), Some(stale.to_vec()));
}
