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

/// Drives one harness over one host and returns its verdict plus everything it printed. The
/// harness owns the exit code; this only reports it.
fn run_harness(bun: &Path, harness: &Path, host: &Path) -> (bool, String) {
    let out = Command::new(bun)
        .arg(harness)
        .arg(host)
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "cannot run {}: {e}\nthe host script is JavaScript, so this test needs \
                 Bun - either build dist/ or put bun on PATH",
                bun.display()
            )
        });
    // Both harnesses report every case they ran, so the whole verdict is worth keeping even
    // on a success path that a later failure would need for context.
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), report)
}

fn probe_path(root: &Path) -> PathBuf {
    root.join("tests/unit/native_runtime/host_probe.cjs")
}

fn adversary_path(root: &Path) -> PathBuf {
    root.join("tests/unit/native_runtime/host_adversary.cjs")
}

#[test]
fn the_host_script_sandbox_policy_holds() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let host = root.join("frontend/scripts/native-host.cjs");
    let bun = bun_path(&root);

    let (ok, report) = run_harness(&bun, &probe_path(&root), &host);
    assert!(ok, "sandbox policy probe failed:\n{report}");
}

/// The other half of the harness, and the half that asks the opposite question. The probe
/// checks named properties at named sites; this corrupts every own member of the six unfrozen
/// prototypes and every global binding, one at a time, and re-runs a suite of gate oracles
/// after each. It does not know what a defect looks like, only what a gate must still
/// answer, which is why it found a hole thirteen hand-written fixes had walked past.
#[test]
fn the_adversary_sweep_finds_no_open_gate() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let host = root.join("frontend/scripts/native-host.cjs");
    let bun = bun_path(&root);

    let (ok, report) = run_harness(&bun, &adversary_path(&root), &host);
    assert!(ok, "a poisoned intrinsic broke a gate:\n{report}");
}

/// The probe's green is only worth what its red is worth, and that was never checked. A
/// containment gate removed from the host has to make it fail. Before this existed, deleting
/// `assertDelete`'s check left the whole suite at 100% pass, because the promises arms only
/// asked whether a thenable came back, never whether it rejected. This test is the one that
/// notices when the probe stops testing.
#[test]
fn the_harness_notices_a_broken_sandbox() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let host = root.join("frontend/scripts/native-host.cjs");
    let bun = bun_path(&root);

    // Anchored on the gate itself: if it is renamed the canary must be rewritten against the
    // new one, not silently skipped.
    const ANCHOR: &str = "function assertDelete(p, dataDirs) {";
    let source = std::fs::read_to_string(&host).expect("cannot read the host script");
    assert!(
        source.contains(ANCHOR),
        "the canary's anchor is gone - point it at the current delete gate"
    );
    let broken = source.replacen(
        ANCHOR,
        &format!("{ANCHOR}\n    return canonicalizeLeafPath(p); // canary: containment removed"),
        1,
    );

    let dir = tempfile::tempdir().expect("cannot create a scratch dir");
    let broken_host = dir.path().join("broken-host.cjs");
    std::fs::write(&broken_host, broken).expect("cannot write the broken host");

    // Both harnesses have to notice, and for different reasons: the probe because it asserts
    // the promises facade REJECTS rather than merely returning a thenable, the sweep because
    // its assertDelete oracle asks the gate directly. Either one going green here would mean
    // that harness had stopped testing.
    let (probe_ok, probe_report) = run_harness(&bun, &probe_path(&root), &broken_host);
    assert!(
        !probe_ok,
        "the probe passed against a host whose delete containment was removed, so it is not \
         testing what it claims:\n{probe_report}"
    );

    let (sweep_ok, sweep_report) = run_harness(&bun, &adversary_path(&root), &broken_host);
    assert!(
        !sweep_ok,
        "the adversary sweep passed against a host whose delete containment was removed, so \
         its oracles are not reaching the gate:\n{sweep_report}"
    );
}

/// The canary above only proves the harnesses notice a gate that wrongly GRANTS. It says nothing
/// about the opposite shape (a gate that throws where it used to answer), and that blind spot
/// was real: the sweep scored every thrown oracle as a pass, so three separate typo-class
/// regressions in the host reported a clean run. This is the canary for that class, and the
/// reason it has to exist separately is that the two defects meet the harness through different
/// code: one lands in `mustDeny`'s "did not throw" branch, the other in the suite runner's catch.
#[test]
fn the_harness_notices_a_gate_that_throws() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let host = root.join("frontend/scripts/native-host.cjs");
    let bun = bun_path(&root);

    // `isSafe` is the cleanest target: exactly one sweep oracle reads it, with no local
    // try/catch, so a throw travels straight to the runner rather than being scored somewhere
    // on the way. Anchored so a rename fails here instead of silently skipping the check.
    const ANCHOR: &str = "function isSafe(id) {";
    let source = std::fs::read_to_string(&host).expect("cannot read the host script");
    assert!(
        source.contains(ANCHOR),
        "the canary's anchor is gone - point it at the current module-classification gate"
    );
    let broken = source.replacen(
        ANCHOR,
        &format!("{ANCHOR}\n    throw new Error(\"canary: gate throws\");"),
        1,
    );

    let dir = tempfile::tempdir().expect("cannot create a scratch dir");
    let broken_host = dir.path().join("throwing-host.cjs");
    std::fs::write(&broken_host, broken).expect("cannot write the broken host");

    let (probe_ok, probe_report) = run_harness(&bun, &probe_path(&root), &broken_host);
    assert!(
        !probe_ok,
        "the probe passed against a host whose module gate throws unconditionally:\n{probe_report}"
    );

    let (sweep_ok, sweep_report) = run_harness(&bun, &adversary_path(&root), &broken_host);
    assert!(
        !sweep_ok,
        "the adversary sweep passed against a host whose module gate throws unconditionally, so \
         it is still scoring a thrown oracle as a pass:\n{sweep_report}"
    );
}
