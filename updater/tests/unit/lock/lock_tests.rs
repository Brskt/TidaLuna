//! Tests for `src/lock.rs`, attached by `#[path]`. They take the real install
//! lock, and fail rather than pass untested while an install holds it.

use super::*;

/// What the second taker must find the lock to be, set in the child the test
/// launches.
const SECOND_TAKER: &str = "TIDALUNAR_TEST_SECOND_TAKER";
const HELD: &str = "held";
const FREE: &str = "free";

/// The second taker must be another process (see `UserNamespace`): the test
/// binary reruns this test alone and requires `1 passed`; a filter that
/// matches nothing still exits 0.
fn second_taker_finds(expect: &str) {
    let module = module_path!()
        .split_once("::")
        .map_or(module_path!(), |(_, rest)| rest);
    let output = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args([
            &format!("{module}::a_held_install_lock_is_contended_and_a_released_one_is_not"),
            "--exact",
            "--test-threads=1",
        ])
        .env(SECOND_TAKER, expect)
        .output()
        .expect("launch the second taker");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "the second taker did not run to a pass ({}):\n{stdout}",
        output.status
    );
}

#[test]
fn a_held_install_lock_is_contended_and_a_released_one_is_not() {
    if let Some(expect) = std::env::var_os(SECOND_TAKER) {
        let got = try_acquire().expect("the lock is reachable");
        match expect.to_str() {
            Some(HELD) => assert!(got.is_none(), "took the lock while another process held it"),
            Some(FREE) => assert!(
                got.is_some(),
                "the lock stayed taken after its holder let it go"
            ),
            other => panic!("unknown expectation {other:?}"),
        }
        return;
    }

    let held = try_acquire()
        .expect("the lock is reachable")
        .expect("no installer or updater of this account is running");
    second_taker_finds(HELD);
    drop(held);
    second_taker_finds(FREE);
}
