//! Tests for `src/platform/auth.rs`, attached to it by `#[path]`.

#[cfg(unix)]
use super::*;

#[cfg(unix)]
#[test]
fn write_private_forces_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pkce_credentials.json");
    // A pre-existing world-readable file must be tightened to 0600.
    std::fs::write(&path, b"old").expect("seed");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).expect("chmod");

    write_private(&path, b"verifier").expect("write");

    let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    assert_eq!(std::fs::read(&path).expect("read"), b"verifier");
}
