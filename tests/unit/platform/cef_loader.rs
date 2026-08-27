//! Tests for `src/platform/cef_loader.rs`, attached to it by `#[path]`.
//!
//! macOS-only, like the module itself: they run on a mac and nowhere else.

use super::*;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

/// The argument vectors under test are byte strings, not `String`s: Chromium hands
/// us whatever the OS gave it.
fn argv(items: &[&str]) -> Vec<OsString> {
    items.iter().map(|a| OsString::from(*a)).collect()
}

/// Lay out a bundle replica and hand back the two directories an executable can
/// run from: the main app's `MacOS/`, and a helper app's own `MacOS/`.
fn bundle(root: &Path) -> (PathBuf, PathBuf) {
    let app = root.join("tidalunar.app");
    let main_dir = app.join("Contents/MacOS");
    let helper_dir = app.join("Contents/Frameworks/tidalunar Helper.app/Contents/MacOS");
    let framework = app.join("Contents/Frameworks/Chromium Embedded Framework.framework");
    fs::create_dir_all(&main_dir).expect("main dir");
    fs::create_dir_all(&helper_dir).expect("helper dir");
    fs::create_dir_all(&framework).expect("framework dir");
    fs::create_dir_all(framework.join("Libraries")).expect("libraries dir");
    fs::write(framework.join("Chromium Embedded Framework"), b"").expect("framework binary");
    fs::write(framework.join("Libraries/libcef_sandbox.dylib"), b"").expect("sandbox library");
    (main_dir, helper_dir)
}

#[test]
fn main_and_helper_reach_the_same_framework() {
    let root = tempfile::tempdir().expect("tempdir");
    let (main_dir, helper_dir) = bundle(root.path());

    let from_main = framework_path(&main_dir, false)
        .canonicalize()
        .expect("from main");
    let from_helper = framework_path(&helper_dir, true)
        .canonicalize()
        .expect("from helper");

    assert_eq!(from_main, from_helper);
    assert!(
        from_main.ends_with("Chromium Embedded Framework.framework/Chromium Embedded Framework")
    );
}

#[test]
fn a_helper_taking_the_main_hop_misses_the_framework() {
    let root = tempfile::tempdir().expect("tempdir");
    let (_, helper_dir) = bundle(root.path());

    // The main-process hop lands inside the helper's own bundle, which holds no
    // framework. This is what the process crashed on before the flag existed.
    assert!(framework_path(&helper_dir, false).canonicalize().is_err());
}

#[test]
fn the_main_process_taking_the_helper_hop_misses_the_framework() {
    let root = tempfile::tempdir().expect("tempdir");
    let (main_dir, _) = bundle(root.path());

    // Three levels up from the main app leaves the bundle entirely.
    assert!(framework_path(&main_dir, true).canonicalize().is_err());
}

#[test]
fn the_chromium_subprocess_switch_selects_the_helper_hop() {
    assert!(is_helper_process(
        argv(&["/path/to/exe", "--type=renderer", "--lang=en-US"]).into_iter()
    ));
}

#[test]
fn a_bare_launch_selects_the_main_hop() {
    assert!(!is_helper_process(
        argv(&["/path/to/exe", "--lang=en-US"]).into_iter()
    ));
}

#[test]
fn a_switch_merely_containing_the_word_type_selects_the_main_hop() {
    // Chromium always spells it `--type=<kind>`; neither of these is that.
    assert!(!is_helper_process(
        argv(&["/path/to/exe", "--type", "--user-agent=x--type=renderer"]).into_iter()
    ));
}

#[test]
fn an_argument_that_is_not_valid_unicode_is_read_without_panicking() {
    // `std::env::args()` unwraps internally and aborts on this input, which is why
    // the predicate takes bytes. The switch after the bad argument still has to be
    // seen, so this covers the reading as well as the surviving.
    let mut items = argv(&["/path/to/exe"]);
    items.push(OsString::from_vec(vec![0x2d, 0x2d, 0xff, 0xfe]));
    items.push(OsString::from("--type=gpu-process"));

    assert!(is_helper_process(items.into_iter()));
}

#[test]
fn a_helper_reaches_the_sandbox_library_beside_the_framework() {
    let root = tempfile::tempdir().expect("tempdir");
    let (_, helper_dir) = bundle(root.path());

    let dylib = sandbox_dylib_path(&helper_dir)
        .canonicalize()
        .expect("sandbox library");
    let framework = framework_path(&helper_dir, true)
        .canonicalize()
        .expect("framework");

    assert!(dylib.ends_with("Libraries/libcef_sandbox.dylib"));
    // Both hops have to land in the same framework bundle, since a subprocess
    // adopts the sandbox and loads the framework from one place.
    assert_eq!(
        dylib.ancestors().nth(2),
        framework.ancestors().nth(1),
        "sandbox library and framework must share one framework bundle"
    );
}
