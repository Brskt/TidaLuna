//! Tests for `src/platform/refusal.rs`, attached by `#[path]`.

use std::io::Write;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{install_launch_guard, panic_message, panic_refusal};

fn refused() -> Result<(), &'static str> {
    Err("refused")
}

#[test]
fn a_panic_message_is_read_whether_it_was_formatted_or_not() {
    let literal = std::panic::catch_unwind(|| panic!("a literal")).unwrap_err();
    let formatted = std::panic::catch_unwind(|| panic!("a {}", "formatted one")).unwrap_err();
    let unwrapped = std::panic::catch_unwind(|| refused().unwrap()).unwrap_err();
    let other = std::panic::catch_unwind(|| std::panic::panic_any(5_u8)).unwrap_err();

    assert_eq!(panic_message(literal.as_ref()), "a literal");
    assert_eq!(panic_message(formatted.as_ref()), "a formatted one");
    assert!(panic_message(unwrapped.as_ref()).contains("refused"));
    assert_eq!(panic_message(other.as_ref()), "no message");
}

#[test]
fn a_panic_refusal_names_the_message_and_where_it_came_from() {
    let with = panic_refusal("boom", Some(("src/db.rs", 254)));
    assert!(
        with.ends_with("cannot continue: boom (src/db.rs:254)"),
        "{with}"
    );

    let without = panic_refusal("boom", None);
    assert!(without.ends_with("cannot continue: boom"), "{without}");
}

// The hook is process-global; the launch guard is exercised in a child process: the test
// binary runs itself on `child_scenario`, which does nothing unless the parent names one.

const CHILD_ENV: &str = "TIDALUNAR_REFUSAL_TEST_SCENARIO";
const CHILD_TEST: &str = "platform::refusal::tests::child_scenario";
const REFUSED_EXIT: i32 = 42;

fn refuse_by_exiting(why: &str) -> ! {
    let _ = writeln!(std::io::stdout(), "REFUSED: {why}");
    std::process::exit(REFUSED_EXIT);
}

fn run_child(scenario: &str) -> std::process::Output {
    Command::new(std::env::current_exe().expect("the test binary has a path"))
        .args(["--exact", CHILD_TEST, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, scenario)
        .output()
        .expect("the test binary starts")
}

#[test]
fn child_scenario() {
    static LAUNCHED: AtomicBool = AtomicBool::new(false);

    let Ok(scenario) = std::env::var(CHILD_ENV) else {
        return;
    };
    install_launch_guard(std::thread::current().id(), &LAUNCHED, refuse_by_exiting);
    match scenario.as_str() {
        "main_thread_panics" => panic!("boom {}", 7),
        "other_thread_panics" => {
            let _ = std::thread::spawn(|| panic!("worker panic")).join();
        }
        "main_thread_panics_after_the_launch" => {
            LAUNCHED.store(true, Ordering::Release);
            let _ = std::panic::catch_unwind(|| panic!("late panic"));
        }
        other => panic!("unknown scenario {other}"),
    }
}

#[test]
fn a_panic_on_the_watched_thread_stops_the_launch_with_its_message() {
    let out = run_child("main_thread_panics");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(out.status.code(), Some(REFUSED_EXIT), "{stdout}");
    assert!(
        stdout.contains("REFUSED: TidaLunar hit an unexpected error"),
        "{stdout}"
    );
    assert!(stdout.contains("boom 7"), "{stdout}");
}

#[test]
fn a_panic_on_another_thread_is_left_to_its_owner() {
    let out = run_child("other_thread_panics");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(out.status.success(), "{stdout}");
    assert!(!stdout.contains("REFUSED"), "{stdout}");
    assert!(
        stdout.contains("1 passed"),
        "the child test did not run: {stdout}"
    );
}

#[test]
fn a_panic_after_the_launch_is_done_is_an_ordinary_panic() {
    let out = run_child("main_thread_panics_after_the_launch");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(out.status.success(), "{stdout}");
    assert!(!stdout.contains("REFUSED"), "{stdout}");
    assert!(
        stdout.contains("1 passed"),
        "the child test did not run: {stdout}"
    );
}
