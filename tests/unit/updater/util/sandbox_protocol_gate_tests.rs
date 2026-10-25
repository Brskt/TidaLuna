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
fn read_system_protocol_reads_the_file_and_fails_closed_on_garbage() {
    for (contents, expected) in [
        (None, None),
        (Some("5\n"), Some(5)),
        (Some("not-a-number\n"), Some(0)),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SANDBOX_PROTOCOL_VERSION");
        if let Some(text) = contents {
            std::fs::write(&path, text).unwrap();
        }
        assert_eq!(
            read_system_sandbox_protocol_from(path.to_str().unwrap()),
            expected,
            "{contents:?}"
        );
    }
}

#[test]
fn the_gate_refuses_only_a_required_protocol_above_the_systems() {
    for (required, system, refusal) in [
        (Some(1), Some(1), None),
        (
            Some(2),
            Some(1),
            Some(["requires sandbox helper protocol 2", "system has 1"]),
        ),
        (None, Some(0), None),
        // tar.gz / dev install: no system protocol file -> gate does not apply.
        (Some(2), None, None),
    ] {
        let result = check_sandbox_protocol(&fixture_manifest(required), system);
        match refusal {
            None => assert!(
                result.is_ok(),
                "required {required:?} against system {system:?}"
            ),
            Some(parts) => {
                let message = format!("{}", result.unwrap_err());
                for part in parts {
                    assert!(message.contains(part), "got: {message}");
                }
            }
        }
    }
}
