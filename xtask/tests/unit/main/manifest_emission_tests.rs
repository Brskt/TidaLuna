//! Tests for `xtask/src/main.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn windows_manifest_omits_protocol_required() {
    let manifest = Manifest {
        version: "0.0.5-alpha".to_string(),
        min_version: "0.0.4-alpha".to_string(),
        target: "windows-amd64".to_string(),
        files: BTreeMap::new(),
        sandbox_protocol_required: None,
        delta_from: None,
    };
    let json = serde_json::to_string(&manifest).unwrap();
    assert!(
        !json.contains("sandbox_protocol_required"),
        "Windows manifest must omit the protocol field; got: {json}"
    );
}

/// The updater refuses a payload whose declared protocol the system bootstrap does not meet,
/// and the field is read from the manifest this function writes. A Linux build that left it
/// out would pass that gate on any bootstrap, however stale. Every other target omits it.
#[test]
fn a_manifest_declares_the_protocol_only_on_linux() {
    let dir = std::env::temp_dir().join(format!("xtask-manifest-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    generate_manifest(&dir).unwrap();
    let json = fs::read_to_string(dir.join("manifest.json")).unwrap();
    fs::remove_dir_all(&dir).unwrap();

    let manifest: Manifest = serde_json::from_str(&json).unwrap();
    let expected = cfg!(target_os = "linux").then_some(LINUX_SANDBOX_PROTOCOL_REQUIRED);
    assert_eq!(manifest.sandbox_protocol_required, expected, "got: {json}");
}
