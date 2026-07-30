//! Tests for `updater/src/main.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn manifest_roundtrip_with_protocol_field() {
    let json = r#"{
            "version": "0.0.5-alpha",
            "min_version": "0.0.4-alpha",
            "target": "linux-amd64",
            "files": {},
            "sandbox_protocol_required": 1
        }"#;
    let m: Manifest = serde_json::from_str(json).unwrap();
    assert_eq!(m.sandbox_protocol_required, Some(1));
    let serialized = serde_json::to_string(&m).unwrap();
    assert!(serialized.contains("\"sandbox_protocol_required\":1"));
}

#[test]
fn manifest_roundtrip_without_protocol_field_defaults_none() {
    let json = r#"{
            "version": "0.0.4-alpha",
            "min_version": "0.0.4-alpha",
            "target": "windows-amd64",
            "files": {}
        }"#;
    let m: Manifest = serde_json::from_str(json).unwrap();
    assert_eq!(m.sandbox_protocol_required, None);
}

#[test]
fn manifest_delta_from_roundtrip() {
    let json = r#"{"version":"0.0.5-alpha","min_version":"0.0.4-alpha","target":"linux-amd64","files":{},"delta_from":"0.0.4-alpha"}"#;
    let m: Manifest = serde_json::from_str(json).unwrap();
    assert_eq!(m.delta_from.as_deref(), Some("0.0.4-alpha"));
}
