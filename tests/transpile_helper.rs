//! The transpile helper, run as the real executable: the request and answer layout below is the
//! one `src/plugins/transpile_helper.rs` speaks.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MARKER: &str = "--tidalunar-transpile-helper";

fn run_helper(source: &str) -> (std::process::Output, Duration) {
    let filename = "p.mts";
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tidalunar"))
        .arg(MARKER)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(&(filename.len() as u32).to_le_bytes())
        .unwrap();
    stdin.write_all(filename.as_bytes()).unwrap();
    stdin.write_all(source.as_bytes()).unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    (output, started.elapsed())
}

/// A plugin bundle of nested brackets overflows oxc's stack, and a stack overflow aborts the
/// process that runs it. Run in the helper, that is an exit status: quick, with no answer. On
/// Windows it is quick only because the helper turns off the crash dialog first.
#[test]
fn a_source_that_crashes_the_transpiler_ends_only_the_helper() {
    let (answered, _) = run_helper("export const x: number = 1;");
    assert!(answered.status.success(), "{:?}", answered.status);
    assert_eq!(
        answered.stdout.first(),
        Some(&0),
        "the marker reached the helper"
    );

    let nested = format!("x={}{};", "[".repeat(100_000), "]".repeat(100_000));
    let (crashed, elapsed) = run_helper(&nested);
    assert!(!crashed.status.success(), "{:?}", crashed.status);
    assert!(crashed.stdout.is_empty(), "a crash left an answer");
    assert!(
        elapsed < Duration::from_secs(5),
        "the crash took {elapsed:?}"
    );
}
