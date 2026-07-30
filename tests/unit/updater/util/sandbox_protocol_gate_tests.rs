//! Tests for `src/updater/util.rs`, attached to it by `#[path]`.
//!
//! Gated on Linux at the declaration site: the sandbox-helper protocol file only
//! exists there.

use super::super::types::Manifest;
use super::*;
use std::collections::BTreeMap;

fn fixture_manifest(required: Option<u32>) -> Manifest {
    Manifest {
        version: "0.0.5-alpha".into(),
        min_version: "0.0.4-alpha".into(),
        target: "linux-amd64".into(),
        files: BTreeMap::new(),
        sandbox_protocol_required: required,
        delta_from: None,
    }
}

#[test]
fn read_system_protocol_missing_file_returns_none() {
    let v = read_system_sandbox_protocol_from("/nonexistent/path/SANDBOX_PROTOCOL_VERSION");
    assert_eq!(v, None);
}

#[test]
fn read_system_protocol_parses_integer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SANDBOX_PROTOCOL_VERSION");
    std::fs::write(&path, "5\n").unwrap();
    let v = read_system_sandbox_protocol_from(path.to_str().unwrap());
    assert_eq!(v, Some(5));
}

#[test]
fn read_system_protocol_corrupted_file_returns_some_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SANDBOX_PROTOCOL_VERSION");
    std::fs::write(&path, "not-a-number\n").unwrap();
    let v = read_system_sandbox_protocol_from(path.to_str().unwrap());
    assert_eq!(v, Some(0));
}

#[test]
fn gate_passes_when_required_le_system() {
    let manifest = fixture_manifest(Some(1));
    let result = check_sandbox_protocol(&manifest, Some(1));
    assert!(result.is_ok());
}

#[test]
fn gate_fails_when_required_gt_system() {
    let manifest = fixture_manifest(Some(2));
    let err = check_sandbox_protocol(&manifest, Some(1)).unwrap_err();
    let s = format!("{err}");
    assert!(s.contains("requires sandbox helper protocol 2"), "got: {s}");
    assert!(s.contains("system has 1"), "got: {s}");
}

#[test]
fn gate_passes_when_field_absent() {
    let manifest = fixture_manifest(None);
    let result = check_sandbox_protocol(&manifest, Some(0));
    assert!(result.is_ok());
}

#[test]
fn gate_skipped_when_no_system_file() {
    // tar.gz / dev install: no system protocol file -> gate does not apply.
    let manifest = fixture_manifest(Some(2));
    let result = check_sandbox_protocol(&manifest, None);
    assert!(result.is_ok());
}
