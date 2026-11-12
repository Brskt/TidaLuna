//! Tests for `updater/src/main.rs`, attached to it by `#[path]`.

use std::collections::BTreeMap;
use std::fs;

use super::{Args, FileEntry, Manifest, plan_updates};

#[test]
fn plan_updates_refuses_a_staged_file_whose_hash_differs_from_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let app_dir = dir.path().join("app");
    let staging_dir = app_dir.join(".update-staging");
    fs::create_dir_all(&staging_dir).unwrap();
    fs::write(staging_dir.join("lib.bin"), b"tampered").unwrap();
    let manifest = Manifest {
        version: "2.0.0".into(),
        min_version: "1.0.0".into(),
        target: String::new(),
        files: BTreeMap::from([(
            "lib.bin".to_string(),
            FileEntry {
                sha256: "0".repeat(64),
                size: 0,
            },
        )]),
        sandbox_protocol_required: None,
        delta_from: None,
    };
    let args = Args {
        pid: 0,
        version: "2.0.0".into(),
        app_dir,
        skip_download: true,
    };

    let err = plan_updates(&args, &manifest, &staging_dir).unwrap_err();
    assert!(err.to_string().contains("hash mismatch"), "got: {err}");
}
