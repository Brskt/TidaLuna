//! Tests for `src/frontend/dir.rs`, attached by `#[path]`.

use std::ffi::OsString;
use std::path::PathBuf;

use super::{FrontendDirError, resolve_frontend_dir};

/// Absolute on the platform under test.
fn absolute(tail: &str) -> PathBuf {
    std::env::temp_dir().join(tail)
}

#[test]
fn a_relative_override_is_refused_not_ignored() {
    let found = resolve_frontend_dir(
        Some(OsString::from("frontend/dist")),
        Ok(absolute("x/tidalunar")),
    );
    assert!(matches!(
        found,
        Err(FrontendDirError::OverrideNotAbsolute(_))
    ));
}
