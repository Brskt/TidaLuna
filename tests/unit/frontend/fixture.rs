//! A front folder on disk for tests, attached to `src/frontend/mod.rs` by `#[path]`.

use std::path::Path;

use super::files::{BUNDLE, LUNA_DEV, LUNA_UI, MANIFEST, NATIVE_HOST};
use super::load::EARLY_RUNTIME_FILES;

/// Writes into `dir` every file of the shipped allowlist, valid as loaded, `frontend.json`
/// naming `version`.
pub(crate) fn write_front(dir: &Path, version: &str) {
    std::fs::write(dir.join(BUNDLE), "console.log('bundle');").expect("bundle");
    std::fs::write(dir.join(LUNA_UI), "export {};").expect("ui");
    std::fs::write(dir.join(LUNA_DEV), "export {};").expect("dev");
    std::fs::write(dir.join(NATIVE_HOST), "'use strict';\n").expect("host");
    std::fs::write(dir.join(MANIFEST), format!("{{\"version\":\"{version}\"}}")).expect("manifest");
    std::fs::create_dir(dir.join("early-runtime")).expect("early dir");
    for name in EARLY_RUNTIME_FILES {
        std::fs::write(dir.join("early-runtime").join(name), "void 0;").expect("early file");
    }
}
