//! Tests for `src/platform/cef_loader.rs`, attached to it by `#[path]`.
//!
//! macOS-only, like the module itself: they run on a mac and nowhere else.

use super::*;
use std::fs;
use std::path::{Path, PathBuf};

/// Fill CEF's dispatch table before `main`; no test has to remember to.
///
/// A test binary has no `bootstrap()`, and a `cef_*` call on the empty table jumps to address
/// zero, whether a test makes it directly or through a product function. dyld runs every entry
/// of `__mod_init_func` before `main`, ahead of libtest's first thread.
#[used]
#[unsafe(link_section = "__DATA,__mod_init_func")]
static LOAD_FRAMEWORK_BEFORE_MAIN: extern "C" fn() = load_framework_before_main;

extern "C" fn load_framework_before_main() {
    ensure_framework_loaded();
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
