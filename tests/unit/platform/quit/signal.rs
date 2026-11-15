//! Tests for `src/platform/quit/signal.rs`, attached by `#[path]`.

use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::time::{Duration, Instant};

use libc::c_int;

use super::install;
use crate::platform::quit::{Phase, enter, requested};

// Signal dispositions are process-global: the test binary runs itself on `child_scenario`,
// which does nothing unless the parent sets the variable.

const CHILD_ENV: &str = "TIDALUNAR_QUIT_SIGNAL_TEST";
const CHILD_TEST: &str = "platform::quit::signal::tests::child_scenario";
const EARLIER_EXIT: c_int = 42;
const SURVIVED: &str = "SURVIVED THE FIRST SIGTERM";

/// Stands for Chromium's SIGTERM handler, ending the process with `_exit(0)`.
extern "C" fn earlier_handler(_: c_int) {
    // SAFETY: `_exit` is async-signal-safe.
    unsafe { libc::_exit(EARLIER_EXIT) }
}

/// `request` spawns the watchdog once it has taken the normal exit.
fn watchdog_started() -> bool {
    std::fs::read_dir("/proc/self/task")
        .expect("procfs lists this process's threads")
        .flatten()
        .any(|task| {
            std::fs::read_to_string(task.path().join("comm"))
                .is_ok_and(|name| name.trim_end() == "quit-watchdog")
        })
}

#[test]
fn child_scenario() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    // SAFETY: a zeroed `sigaction` is a valid empty one; the handler is async-signal-safe.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = earlier_handler as extern "C" fn(c_int) as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
    }
    install();
    enter(Phase::Winding);

    // SAFETY: SIGTERM now runs `on_signal`, and it returns.
    unsafe { libc::raise(libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !watchdog_started() {
        assert!(
            Instant::now() < deadline,
            "the first SIGTERM never reached `request`"
        );
        std::thread::yield_now();
    }
    let _ = writeln!(std::io::stdout(), "{SURVIVED}");
    // SAFETY: SIGTERM is back to its default action, which ends the process.
    unsafe { libc::raise(libc::SIGTERM) };
    unreachable!("a second SIGTERM ends the process");
}

#[test]
fn our_handler_replaces_the_earlier_one_and_serves_one_signal() {
    let out = Command::new(std::env::current_exe().expect("the test binary has a path"))
        .args(["--exact", CHILD_TEST, "--nocapture"])
        .env(CHILD_ENV, "1")
        .output()
        .expect("the test binary starts");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains(SURVIVED),
        "the first SIGTERM did not take the normal exit ({:?}, {EARLIER_EXIT} = the earlier handler ran)\n{stdout}",
        out.status
    );
    assert_eq!(
        out.status.signal(),
        Some(libc::SIGTERM),
        "the second SIGTERM ends the process at once ({:?})",
        out.status
    );
}

// The handler notes the request itself: a renderer killed by the same group signal is reported
// on the UI thread, which `quit-signal` does not wait for.

const NOTED_ENV: &str = "TIDALUNAR_QUIT_NOTED_TEST";
const NOTED_TEST: &str = "platform::quit::signal::tests::child_handler_alone";

#[test]
fn child_handler_alone() {
    if std::env::var_os(NOTED_ENV).is_none() {
        return;
    }
    // Without `install` there is no pipe and no reader: only the handler can note the request.
    super::on_signal(libc::SIGTERM);
    let _ = writeln!(std::io::stdout(), "requested: {}", requested());
}

#[test]
fn the_handler_notes_the_request_without_the_reader() {
    let out = Command::new(std::env::current_exe().expect("the test binary has a path"))
        .args(["--exact", NOTED_TEST, "--nocapture"])
        .env(NOTED_ENV, "1")
        .output()
        .expect("the test binary starts");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success() && stdout.contains("requested: true"),
        "{:?}\n{stdout}",
        out.status
    );
}

// One sender sends two kinds (systemd: SIGTERM then SIGHUP; a closing terminal: SIGHUP twice).
// Each sequence must leave the normal exit running; the parent passes the sequence in the
// environment.

const SEQUENCE_ENV: &str = "TIDALUNAR_QUIT_SIGNAL_SEQUENCE";
const SEQUENCE_TEST: &str = "platform::quit::signal::tests::child_sequence";
const SURVIVED_SEQUENCE: &str = "SURVIVED THE SEQUENCE";

fn signal_named(name: &str) -> c_int {
    match name {
        "TERM" => libc::SIGTERM,
        "HUP" => libc::SIGHUP,
        other => panic!("unknown signal {other}"),
    }
}

/// `quit-signal` asleep in `read` has answered every signal raised so far: each byte is in the
/// pipe before `raise` returns, and `read` sleeps only on an empty pipe.
fn reader_asleep_in_read() -> bool {
    std::fs::read_dir("/proc/self/task")
        .expect("procfs lists this process's threads")
        .flatten()
        .any(|task| {
            let path = task.path();
            let read = |name: &str| std::fs::read_to_string(path.join(name)).unwrap_or_default();
            // `stat` is "tid (comm) state ...", the comm itself may hold spaces or parentheses.
            let stat = read("stat");
            let asleep = stat
                .rsplit_once(") ")
                .is_some_and(|(_, rest)| rest.starts_with('S'));
            read("comm").trim_end() == "quit-signal"
                && asleep
                && read("syscall")
                    .split(' ')
                    .next()
                    .and_then(|n| n.parse::<libc::c_long>().ok())
                    == Some(libc::SYS_read)
        })
}

#[test]
fn child_sequence() {
    let Some(sequence) = std::env::var_os(SEQUENCE_ENV) else {
        return;
    };
    let sequence: Vec<c_int> = sequence
        .to_str()
        .expect("the sequence is text")
        .split(',')
        .map(signal_named)
        .collect();
    install();
    enter(Phase::Winding);

    for sig in sequence {
        // SAFETY: the signal runs `on_signal`, and it returns.
        unsafe { libc::raise(sig) };
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !reader_asleep_in_read() {
        assert!(
            Instant::now() < deadline,
            "the signals were never all answered"
        );
        std::thread::yield_now();
    }
    let _ = writeln!(std::io::stdout(), "{SURVIVED_SEQUENCE}");
    // SAFETY: every sequence holds a SIGTERM, back to its default action, which ends the process.
    unsafe { libc::raise(libc::SIGTERM) };
    unreachable!("a second SIGTERM ends the process");
}

fn sequence_ends_by_the_last_term(sequence: &str) {
    let out = Command::new(std::env::current_exe().expect("the test binary has a path"))
        .args(["--exact", SEQUENCE_TEST, "--nocapture"])
        .env(SEQUENCE_ENV, sequence)
        .output()
        .expect("the test binary starts");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains(SURVIVED_SEQUENCE),
        "{sequence} ended the process before the normal exit ({:?})\n{stdout}",
        out.status
    );
    assert_eq!(
        out.status.signal(),
        Some(libc::SIGTERM),
        "only the SIGTERM sent after the marker ends the process ({:?})",
        out.status
    );
}

#[test]
fn a_hangup_after_a_term_does_not_end_the_exit() {
    sequence_ends_by_the_last_term("TERM,HUP");
}

#[test]
fn a_repeated_hangup_does_not_end_the_exit() {
    sequence_ends_by_the_last_term("HUP,HUP,TERM");
}
