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

/// The variable names the base when it is an absolute directory, else the fallback does. A
/// variable that is not text is refused and not replaced: it names where the data is, and falling
/// back would move it.
#[test]
fn the_base_is_the_variable_else_the_fallback_when_absolute_and_text() {
    let not_text = not_utf8();
    assert!(
        not_text.to_str().is_none(),
        "{not_text:?} is UTF-8, the test proves nothing"
    );
    type Fallback = fn() -> Option<PathBuf>;
    let rows: [(&str, Option<OsString>, Fallback, Option<PathBuf>); 9] = [
        (
            "an absolute variable is the base and the fallback is not asked",
            Some(absolute("home").into_os_string()),
            untouched,
            Some(absolute("home")),
        ),
        (
            "a missing variable leaves it to the fallback",
            None,
            || Some(absolute("from-the-account")),
            Some(absolute("from-the-account")),
        ),
        (
            "an empty variable leaves it to the fallback",
            Some(OsString::new()),
            || Some(absolute("from-the-account")),
            Some(absolute("from-the-account")),
        ),
        (
            "a dot is no directory",
            Some(OsString::from(".")),
            || Some(absolute("from-the-account")),
            Some(absolute("from-the-account")),
        ),
        (
            "a relative path is no directory",
            Some(OsString::from("relative/home")),
            || Some(absolute("from-the-account")),
            Some(absolute("from-the-account")),
        ),
        (
            "a variable that is not text is refused and not replaced",
            Some(not_text.into_os_string()),
            untouched,
            None,
        ),
        (
            "a fallback that is not text is refused",
            None,
            || Some(not_utf8()),
            None,
        ),
        ("nothing anywhere resolves to nothing", None, || None, None),
        (
            "nothing absolute anywhere resolves to nothing",
            Some(OsString::from(".")),
            || Some(PathBuf::from("relative")),
            None,
        ),
    ];
    for (what, env, fallback, expected) in rows {
        assert_eq!(resolve_base(env, fallback), expected, "{what}");
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

#[cfg(not(target_os = "macos"))]
#[test]
fn the_install_dir_is_the_executables_folder_when_it_is_named_and_text() {
    use super::{InstallDir, install_dir};
    let dir = absolute("tidalunar");
    for (what, exe, expected) in [
        (
            "the folder holding the executable",
            Ok(dir.join("tidalunar")),
            InstallDir::Found(dir),
        ),
        (
            "an executable the system cannot name",
            Err(std::io::Error::other("no /proc")),
            InstallDir::Unknown,
        ),
        (
            "a folder that is not text",
            Ok(not_utf8().join("tidalunar")),
            InstallDir::NotText,
        ),
        (
            "a file name that is not text",
            Ok(not_utf8()),
            InstallDir::NotText,
        ),
    ] {
        assert_eq!(install_dir(exe), expected, "{what}");
    }
}
