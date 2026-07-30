//! Tests for `xtask/src/main.rs`, attached to it by `#[path]`.

use super::*;
use std::collections::BTreeMap;

fn entry(sha: &str) -> FileEntry {
    FileEntry {
        sha256: sha.to_string(),
        size: 1,
    }
}

fn manifest_with(files: &[(&str, &str)]) -> Manifest {
    Manifest {
        version: "0.0.5-alpha".into(),
        min_version: "0.0.4-alpha".into(),
        target: "linux-amd64".into(),
        files: files
            .iter()
            .map(|(p, s)| (p.to_string(), entry(s)))
            .collect::<BTreeMap<_, _>>(),
        sandbox_protocol_required: None,
        delta_from: None,
    }
}

#[test]
fn changed_set_includes_new_and_modified_only() {
    let old = manifest_with(&[("a", "h1"), ("b", "h2"), ("gone", "h3")]);
    let new = manifest_with(&[("a", "h1"), ("b", "h2_changed"), ("c", "h4")]);
    let mut changed = delta_changed_files(&old, &new);
    changed.sort();
    assert_eq!(changed, vec!["b".to_string(), "c".to_string()]);
}
