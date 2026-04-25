use std::fs;
use std::process::Command;

fn write_manifest(path: &std::path::Path, version: &str, files: &[(&str, u64)]) {
    let entries: serde_json::Map<String, serde_json::Value> = files
        .iter()
        .map(|(name, size)| {
            (
                (*name).into(),
                serde_json::json!({ "sha256": "00", "size": size }),
            )
        })
        .collect();
    let manifest = serde_json::json!({
        "version": version,
        "min_version": "0.0.0",
        "target": "windows-amd64",
        "files": entries,
    });
    fs::write(path, manifest.to_string()).unwrap();
}

#[test]
fn cleanup_removes_only_old_only_files() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path();

    fs::write(app.join("kept.dat"), b"shared").unwrap();
    fs::write(app.join("removed.dat"), b"old-only").unwrap();
    write_manifest(
        &app.join("old.json"),
        "0.0.1",
        &[("kept.dat", 6), ("removed.dat", 8)],
    );
    write_manifest(&app.join("new.json"), "0.0.2", &[("kept.dat", 6)]);

    let exe = env!("CARGO_BIN_EXE_updater");
    let out = Command::new(exe)
        .args([
            "--cleanup-stale",
            "--app-dir",
            &app.display().to_string(),
            "--old-manifest",
            &app.join("old.json").display().to_string(),
            "--new-manifest",
            &app.join("new.json").display().to_string(),
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "expected success: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !app.join("removed.dat").exists(),
        "removed.dat should be gone"
    );
    assert!(app.join("kept.dat").exists(), "kept.dat should survive");
}

#[test]
fn cleanup_no_op_on_first_install() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path();
    write_manifest(&app.join("new.json"), "0.0.1", &[("kept.dat", 6)]);

    let exe = env!("CARGO_BIN_EXE_updater");
    let status = Command::new(exe)
        .args([
            "--cleanup-stale",
            "--app-dir",
            &app.display().to_string(),
            "--old-manifest",
            &app.join("nonexistent.json").display().to_string(),
            "--new-manifest",
            &app.join("new.json").display().to_string(),
        ])
        .status()
        .unwrap();
    assert!(
        status.success(),
        "missing old manifest should be no-op success"
    );
}

#[test]
fn cleanup_rejects_path_traversal() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path();
    let outside = dir.path().parent().unwrap().join("evil.dat");
    fs::write(&outside, b"protected").unwrap();

    write_manifest(&app.join("old.json"), "0.0.1", &[("../evil.dat", 9)]);
    write_manifest(&app.join("new.json"), "0.0.2", &[]);

    let exe = env!("CARGO_BIN_EXE_updater");
    let _ = Command::new(exe)
        .args([
            "--cleanup-stale",
            "--app-dir",
            &app.display().to_string(),
            "--old-manifest",
            &app.join("old.json").display().to_string(),
            "--new-manifest",
            &app.join("new.json").display().to_string(),
        ])
        .status()
        .unwrap();

    assert!(
        outside.exists(),
        "is_safe_relative_path must block ../ deletes"
    );
    fs::remove_file(&outside).ok();
}

#[test]
fn cleanup_unknown_arg_exits_nonzero() {
    let exe = env!("CARGO_BIN_EXE_updater");
    let status = Command::new(exe)
        .args(["--cleanup-stale", "--bogus"])
        .status()
        .unwrap();
    assert!(!status.success(), "unknown arg should exit non-zero");
}
