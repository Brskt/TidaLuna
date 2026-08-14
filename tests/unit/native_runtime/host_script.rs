//! Tests for the sandbox policy in `frontend/scripts/native-host.cjs`, attached to
//! `src/native_runtime/mod.rs` by `#[path]`.
//!
//! The assertions live in `host_probe.cjs` rather than in a string literal here: the
//! subject is JavaScript, and embedded in Rust it would be neither formatted nor
//! parsed until it ran. This drives Bun over the probe and reports its verdict.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolves Bun through `find_binary` against `dist/`, the same lookup and the same
/// layout the app uses at runtime, so the probe runs the shipped interpreter when one
/// is built. Falls back to the name alone - production refuses that on purpose, but a
/// checkout with no `dist/` still has to be able to run its own tests.
fn bun_path(root: &Path) -> PathBuf {
    let name = if cfg!(windows) { "bun.exe" } else { "bun" };
    super::find_binary(&root.join("dist"), name).unwrap_or_else(|| PathBuf::from(name))
}

#[test]
fn the_host_script_sandbox_policy_holds() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let probe = root.join("tests/unit/native_runtime/host_probe.cjs");
    let host = root.join("frontend/scripts/native-host.cjs");
    let bun = bun_path(&root);

    let out = Command::new(&bun)
        .arg(&probe)
        .arg(&host)
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "cannot run {}: {e}\nthe host script is JavaScript, so this test needs \
                 Bun - either build dist/ or put bun on PATH",
                bun.display()
            )
        });

    // The probe reports every case it ran, so the whole verdict is worth printing
    // even on success paths that a later failure would need for context.
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "sandbox policy probe failed:\n{report}"
    );
}
