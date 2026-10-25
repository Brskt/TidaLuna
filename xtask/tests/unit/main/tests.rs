//! Tests for `xtask/src/main.rs`, attached to it by `#[path]`.

use super::copy_front;
use super::front_files::TOP_FILES;

/// A fresh directory under the system temp dir; xtask carries no `tempfile`.
fn scratch(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("xtask-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A script added to `frontend/early-runtime/` reaches the bundle without a second list to update
/// (the app refuses to start when a script is missing).
#[test]
fn copy_front_takes_every_early_runtime_script_the_build_produced() {
    let src = scratch("front-src");
    for top in TOP_FILES {
        std::fs::write(src.join(top), "x").unwrap();
    }
    std::fs::create_dir(src.join("early-runtime")).unwrap();
    for script in ["a_known.js", "a_new_one.js"] {
        std::fs::write(src.join("early-runtime").join(script), "x").unwrap();
    }
    std::fs::write(src.join("quickjs-dom.js"), "stale").unwrap();
    let out = scratch("front-dst");
    let dst = out.join("frontend");

    let copied = copy_front(&src, &dst);
    let new_one = dst.join("early-runtime/a_new_one.js").is_file();
    let known = dst.join("early-runtime/a_known.js").is_file();
    let bundle = dst.join("bundle.js").is_file();
    let stale = dst.join("quickjs-dom.js").exists();
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&out);

    copied.expect("copies");
    assert!(new_one && known && bundle);
    assert!(!stale, "only the allowlist ships");
}
