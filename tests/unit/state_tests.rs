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

/// Absolute, with a last component that is not UTF-8: a Latin-1 byte on Unix.
#[cfg(unix)]
fn not_utf8() -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    absolute("x").join(std::ffi::OsStr::from_bytes(b"Jos\xe9"))
}

/// Absolute, with a last component that is not UTF-8: a lone surrogate on Windows.
#[cfg(windows)]
fn not_utf8() -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    absolute("x").join(OsString::from_wide(&[u16::from(b'J'), 0xD800]))
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
fn a_variable_that_is_not_utf8_is_refused_and_not_replaced() {
    let dir = not_utf8();
    assert!(
        dir.to_str().is_none(),
        "{dir:?} is UTF-8, the test proves nothing"
    );
    // The variable names where the data is: falling back would move it.
    assert_eq!(resolve_base(Some(dir.into_os_string()), untouched), None);
}

#[test]
fn a_fallback_that_is_not_utf8_is_refused() {
    assert_eq!(resolve_base(None, || Some(not_utf8())), None);
}

#[cfg(not(target_os = "macos"))]
#[test]
fn the_install_dir_is_the_folder_holding_the_executable() {
    use super::{InstallDir, install_dir};
    let dir = absolute("tidalunar");
    assert_eq!(
        install_dir(Ok(dir.join("tidalunar"))),
        InstallDir::Found(dir)
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn an_executable_the_system_cannot_name_leaves_the_install_dir_unknown() {
    use super::{InstallDir, install_dir};
    assert_eq!(
        install_dir(Err(std::io::Error::other("no /proc"))),
        InstallDir::Unknown
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn an_install_path_that_is_not_utf8_is_not_text_in_its_folder_or_its_name() {
    use super::{InstallDir, install_dir};
    // In the folder, then in the file name itself.
    let in_folder = not_utf8().join("tidalunar");
    let in_name = not_utf8();
    for exe in [in_folder, in_name] {
        assert_eq!(install_dir(Ok(exe.clone())), InstallDir::NotText, "{exe:?}");
    }
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
