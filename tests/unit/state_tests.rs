//! Tests for `src/state.rs`, attached by `#[path]`.

use std::ffi::OsString;
use std::path::PathBuf;

use super::resolve_base;

/// Absolute on the platform under test.
fn absolute(tail: &str) -> PathBuf {
    std::env::temp_dir().join(tail)
}

fn untouched() -> Option<PathBuf> {
    panic!("the fallback was consulted although the variable named a directory")
}

#[test]
fn an_absolute_variable_is_the_base_and_nothing_else_is_asked() {
    let dir = absolute("home");
    assert_eq!(
        resolve_base(Some(OsString::from(&dir)), untouched),
        Some(dir)
    );
}

#[test]
fn a_missing_or_empty_variable_leaves_it_to_the_fallback() {
    let dir = absolute("from-the-account");
    assert_eq!(resolve_base(None, || Some(dir.clone())), Some(dir.clone()));
    assert_eq!(
        resolve_base(Some(OsString::new()), || Some(dir.clone())),
        Some(dir)
    );
}

#[test]
fn a_relative_variable_is_no_directory_at_all() {
    let dir = absolute("from-the-account");
    for relative in [".", "relative/home"] {
        assert_eq!(
            resolve_base(Some(OsString::from(relative)), || Some(dir.clone())),
            Some(dir.clone()),
            "{relative:?} was taken as a base"
        );
    }
}

#[cfg(target_os = "windows")]
#[test]
fn the_shell_names_local_app_data_as_an_absolute_directory() {
    // The fallback's only live caller is a launch with LOCALAPPDATA missing; this is
    // where the call itself is exercised.
    let dir = super::known_local_app_data().expect("the shell names LocalAppData");
    assert!(dir.is_absolute(), "{dir:?} is not absolute");
    assert!(dir.is_dir(), "{dir:?} is not a directory");
}

#[test]
fn nothing_absolute_anywhere_resolves_to_nothing() {
    assert_eq!(resolve_base(None, || None), None);
    assert_eq!(
        resolve_base(Some(OsString::from(".")), || Some(PathBuf::from(
            "relative"
        ))),
        None
    );
}
