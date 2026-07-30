//! Tests for `xtask/src/main.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn linux_manifest_emits_protocol_required() {
    let manifest = Manifest {
        version: "0.0.5-alpha".to_string(),
        min_version: "0.0.4-alpha".to_string(),
        target: "linux-amd64".to_string(),
        files: BTreeMap::new(),
        sandbox_protocol_required: Some(LINUX_SANDBOX_PROTOCOL_REQUIRED),
        delta_from: None,
    };
    let json = serde_json::to_string(&manifest).unwrap();
    assert!(
        json.contains("\"sandbox_protocol_required\":1"),
        "Linux manifest must carry the protocol field; got: {json}"
    );
}

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
