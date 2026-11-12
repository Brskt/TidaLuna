//! Tests for `updater/src/manifest.rs`, attached to it by `#[path]`.

use std::collections::BTreeMap;
use std::fs;

use super::verify_delta_base;
use crate::{FileEntry, Manifest};

#[test]
fn verify_delta_base_rejects_an_omitted_file_whose_local_copy_differs() {
    let dir = tempfile::tempdir().unwrap();
    let app_dir = dir.path().join("app");
    let staging_dir = app_dir.join(".update-staging");
    fs::create_dir_all(&staging_dir).unwrap();
    fs::write(app_dir.join("lib.bin"), b"older build").unwrap();
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

    let err = verify_delta_base(&manifest, &app_dir, &staging_dir).unwrap_err();
    assert!(err.contains("does not match"), "got: {err}");
}
