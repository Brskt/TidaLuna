//! The files the app reads from the top of its front folder, which xtask copies into a bundle.
//!
//! Shared with xtask by `#[path]`. Each includer defines `emit_top_files` before the `mod` line
//! and emits only what it reads from this one list: the app one constant per file, xtask the list
//! it ships. An item one side never reads would be dead code there.

emit_top_files! {
    MANIFEST = "frontend.json",
    NATIVE_HOST = "native-host.cjs",
    BUNDLE = "bundle.js",
    LUNA_UI = "luna-ui.mjs",
    LUNA_DEV = "luna-dev.mjs",
}
