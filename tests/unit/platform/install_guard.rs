//! Tests for `src/platform/install_guard.rs`, attached by `#[path]`.

use std::path::{Path, PathBuf};

use super::{FileId, Place, Protected};

/// An install folder laid out like a shipped one, holding the native data dirs as the deb does.
fn install() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(root.path().join("frontend")).expect("frontend");
    std::fs::write(root.path().join("frontend/bundle.js"), "x").expect("bundle");
    std::fs::create_dir(root.path().join("bin")).expect("bin");
    std::fs::write(root.path().join("tidalunar"), "x").expect("exe");
    std::fs::create_dir_all(root.path().join("native/plugin")).expect("data dir");
    root
}

fn guard(root: &Path) -> Protected {
    Protected::new(root, &root.join("native"))
}

/// Windows looks for a DLL in the exe's folder before any other; whatever lands there runs with
/// the app, as a rewritten exe does. A fresh install has no update journal yet, and creating one
/// is refused the same way.
#[test]
fn everything_in_the_install_folder_is_covered() {
    let root = install();
    let guard = guard(root.path());
    for inside in [
        "frontend",
        "frontend/bundle.js",
        "frontend/a/b/new.js",
        "version.dll",
        "plugins.db",
        ".update-journal.json",
        "new/x.txt",
    ] {
        assert!(guard.covers(&root.path().join(inside)), "{inside}");
    }
}

#[test]
fn a_sibling_sharing_the_data_dirs_prefix_is_covered() {
    let root = install();
    std::fs::create_dir(root.path().join("native-old")).expect("sibling");
    assert!(guard(root.path()).covers(&root.path().join("native-old/x")));
}

/// The data dirs, the folder holding them (the first native plugin to register creates it) and the
/// folder holding the install (`$HOME`, `%LOCALAPPDATA%`) stay the user's.
#[test]
fn the_users_folders_are_not_covered() {
    let root = install();
    let guard = guard(root.path());
    let outside = [
        root.path().join("native/plugin/x"),
        root.path().join("native"),
        root.path().parent().expect("tempdir parent").to_path_buf(),
    ];
    for path in outside {
        assert!(!guard.covers(&path), "{}", path.display());
    }
}

/// Windows lays the data dir beside the install folder, which therefore stays whole.
#[test]
fn data_dirs_outside_leave_the_whole_folder_covered() {
    let parent = tempfile::tempdir().expect("tempdir");
    let root = parent.path().join("install");
    let data = parent.path().join("data/native");
    std::fs::create_dir_all(&root).expect("install");
    std::fs::create_dir_all(data.join("plugin")).expect("data dir");
    let guard = Protected::new(&root, &data);
    assert!(guard.covers(&root.join("x.dll")));
    assert!(!guard.covers(&data.join("plugin/x")));
    assert!(!guard.covers(&parent.path().join("data/x")));
}

#[cfg(unix)]
#[test]
fn a_symlink_into_it_is_covered() {
    let root = install();
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let link = elsewhere.path().join("link");
    std::os::unix::fs::symlink(root.path().join("frontend"), &link).expect("symlink");
    assert!(guard(root.path()).covers(&link.join("x")));
}

/// A data dir is judged where its path leads, not where it is spelled.
#[cfg(unix)]
#[test]
fn a_symlink_from_a_data_dir_into_the_install_folder_is_covered() {
    let root = install();
    let link = root.path().join("native/plugin/link");
    std::os::unix::fs::symlink(root.path().join("frontend"), &link).expect("symlink");
    assert!(guard(root.path()).covers(&link.join("x")));
}

#[cfg(windows)]
#[test]
fn a_junction_into_it_is_covered() {
    let root = install();
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let link = elsewhere.path().join("link");
    let made = std::process::Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(&link)
        .arg(root.path().join("frontend"))
        .status()
        .expect("cmd /C mklink");
    assert!(made.success(), "mklink /J failed: {made}");
    assert!(guard(root.path()).covers(&link.join("x")));
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn a_case_variant_is_covered() {
    let root = install();
    assert!(guard(root.path()).covers(&root.path().join("FRONTEND/x")));
}

/// A file system that cannot report file ids (exFAT, some network redirectors) must not switch the
/// guard off; the resolved path under the resolved install folder still decides.
#[test]
fn without_file_ids_the_canonical_path_still_covers() {
    let root = install();
    let by_path = |path: PathBuf| Place {
        id: None,
        path: Some(path.canonicalize().expect("canonical")),
    };
    let guard = Protected {
        root: by_path(root.path().to_path_buf()),
        except: by_path(root.path().join("native")),
    };
    assert!(guard.covers(&root.path().join("frontend/x")));
    assert!(guard.covers(&root.path().join(".update-journal.json")));
    assert!(!guard.covers(&root.path().join("native/plugin/x")));
}

/// Win32 strips a trailing dot or space and reads `name:stream` as a stream of `name`: a spelling
/// that names another entry than it reads is refused rather than judged.
#[cfg(windows)]
#[test]
fn a_windows_alias_spelling_is_refused() {
    let root = install();
    let guard = guard(root.path());
    for alias in [
        ".update-journal.json.",
        ".update-journal.json ",
        ".update-journal.json:evil",
    ] {
        assert!(guard.covers(&root.path().join(alias)), "{alias:?}");
    }
    assert!(guard.covers(&root.path().join("frontend.").join("x")));
}

#[test]
fn two_paths_to_one_directory_share_a_file_id() {
    let root = install();
    let direct = FileId::of(&root.path().join("frontend")).expect("id");
    let dotted = FileId::of(&root.path().join("frontend/.")).expect("id");
    assert_eq!(direct, dotted);
}

#[test]
fn a_shipped_exe_names_its_app() {
    let app = tempfile::tempdir().expect("tempdir");
    let bundle = app.path().join("TidaLunar.app");
    std::fs::create_dir_all(bundle.join("Contents/MacOS")).expect("bundle");
    std::fs::write(bundle.join("Contents/MacOS/tidalunar"), "x").expect("exe");
    assert_eq!(
        super::bundle_root(&bundle.join("Contents/MacOS/tidalunar")),
        Some(bundle.canonicalize().expect("canonical"))
    );
}

/// `cargo run` on macOS: the third ancestor of `~/target/debug/tidalunar` is `$HOME`.
#[test]
fn an_exe_outside_any_app_names_its_own_folder() {
    let home = tempfile::tempdir().expect("tempdir");
    let debug = home.path().join("target/debug");
    std::fs::create_dir_all(&debug).expect("target");
    std::fs::write(debug.join("tidalunar"), "x").expect("exe");
    assert_eq!(
        super::bundle_root(&debug.join("tidalunar")),
        Some(debug.canonicalize().expect("canonical"))
    );
}

/// dyld reports the path the exe was launched by, a symlink to it included.
#[cfg(unix)]
#[test]
fn an_exe_reached_through_a_symlink_names_the_app_it_lives_in() {
    let root = tempfile::tempdir().expect("tempdir");
    let bundle = root.path().join("TidaLunar.app");
    std::fs::create_dir_all(bundle.join("Contents/MacOS")).expect("bundle");
    std::fs::write(bundle.join("Contents/MacOS/tidalunar"), "x").expect("exe");
    std::fs::create_dir(root.path().join("bin")).expect("bin");
    let link = root.path().join("bin/tidalunar");
    std::os::unix::fs::symlink(bundle.join("Contents/MacOS/tidalunar"), &link).expect("symlink");
    assert_eq!(
        super::bundle_root(&link),
        Some(bundle.canonicalize().expect("canonical"))
    );
}
