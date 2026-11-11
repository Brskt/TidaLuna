//! Tests for `src/updater/util.rs`, attached to it by `#[path]`.

use super::{is_newer, meets_min_version};

#[test]
fn is_newer_orders_versions() {
    for (remote, current, newer) in [
        ("0.0.10-alpha", "0.0.9-alpha", true),
        ("0.0.9-alpha", "0.0.10-alpha", false),
        // SemVer 2.0: 0.0.9 > 0.0.9-alpha
        ("0.0.9", "0.0.9-alpha", true),
        ("0.0.9-alpha", "0.0.9", false),
        // Numeric identifier compare, not string.
        ("0.0.9-alpha.10", "0.0.9-alpha.2", true),
        ("0.0.9-alpha.2", "0.0.9-alpha.10", false),
        ("0.0.9-alpha", "0.0.9-alpha", false),
        // Unparseable fails safe.
        ("garbage", "0.0.9-alpha", false),
        ("0.0.9-alpha", "not-a-version", false),
        // Raw SemVer says alpha.dev.5 > alpha; the dev-aware key inverts that, bare releases too.
        ("0.0.14-alpha", "0.0.14-alpha.dev.5", true),
        ("0.0.14-alpha.dev.5", "0.0.14-alpha", false),
        ("0.1.0", "0.1.0-dev.3", true),
        ("0.1.0-dev.3", "0.1.0", false),
        ("0.0.14-alpha.dev.10", "0.0.14-alpha.dev.9", true),
        ("0.0.14-alpha.dev.9", "0.0.14-alpha.dev.10", false),
        ("0.0.14-alpha.dev.1", "0.0.13-alpha", true),
        // Phase change wins.
        ("0.0.20-beta", "0.0.20-alpha.dev.7", true),
        ("0.0.13-alpha", "0.0.14-alpha.dev.1", false),
        // A trailing identifier pair that is not `dev.N` stays raw SemVer.
        ("0.0.9-alpha.rc.2", "0.0.9-alpha", true),
    ] {
        assert_eq!(
            is_newer(remote, current),
            newer,
            "is_newer({remote:?}, {current:?})"
        );
    }
}

#[test]
fn meets_min_version_orders_versions_and_fails_closed() {
    for (installed, floor, meets) in [
        ("0.0.9-alpha", "0.0.0", true),
        ("0.0.9-alpha", "0.0.8-alpha", true),
        ("0.0.8-alpha", "0.0.8-alpha", true),
        // An empty or unparseable floor blocks.
        ("0.0.9-alpha", "", false),
        ("0.0.9-alpha", "garbage", false),
        // A dev build sits below its promoted base and misses that floor.
        ("0.0.14-alpha.dev.5", "0.0.14-alpha", false),
        ("0.0.14-alpha", "0.0.14-alpha.dev.5", true),
    ] {
        assert_eq!(
            meets_min_version(installed, floor),
            meets,
            "meets_min_version({installed:?}, {floor:?})"
        );
    }
}

/// Linux only: the sandbox-helper protocol file only exists there.
#[cfg(target_os = "linux")]
mod sandbox_protocol {
    use super::super::super::types::Manifest;
    use super::super::*;
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
}
