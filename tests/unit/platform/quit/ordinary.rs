//! Tests for `src/platform/quit/mod.rs`, attached by `#[path]`.

use std::io::Write;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::Duration;

use super::{FLUSH_BOUND, begin_winding_with, last_words_on};

// The exit's deadline is process-global: the test binary runs itself on one child per case,
// which does nothing unless the parent names that case. The child then stands for an exit that
// never finishes; only the deadline can end it before the marker.

const CASE_ENV: &str = "TIDALUNAR_QUIT_ORDINARY_CASE";
const CHILD_TEST: &str = "platform::quit::tests::child_case";
const NEVER_FINISHED: &str = "THE EXIT FINISHED";
const ORDINARY_STARTED: &str = "ORDINARY EXIT STARTED";

#[test]
fn child_case() {
    let Some(case) = std::env::var_os(CASE_ENV) else {
        return;
    };
    let case = case.to_str().expect("the case is text");
    match case {
        #[cfg(target_os = "linux")]
        "channel_engaged" => {
            let _lock = crate::platform::app_lock::acquire_or_signal(None)
                .expect("the child takes the lock in its own data directory");
            begin_winding_with(Duration::from_secs(30));
            let _ = writeln!(std::io::stdout(), "{CHANNEL_READY}");
            // The lock lives until the parent ends the child.
            std::thread::sleep(Duration::from_secs(30));
            return;
        }
        "ordinary" => {
            let _ = writeln!(std::io::stdout(), "{ORDINARY_STARTED}");
            begin_winding_with(Duration::from_millis(200));
        }
        #[cfg(unix)]
        "stderr_wedged" => {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicU64, Ordering};
            // A stderr nobody reads; this thread fills the pipe, then blocks inside the write.
            let chunks = Arc::new(AtomicU64::new(0));
            let progress = Arc::clone(&chunks);
            std::thread::spawn(move || {
                let mut stderr = std::io::stderr();
                while stderr.write_all(&[b'x'; 4096]).is_ok() {
                    progress.fetch_add(1, Ordering::Relaxed);
                }
            });
            // Wedged once the count stops moving. A stall taken for the wedge can only let a run
            // pass without testing anything, never fail one.
            let mut seen = u64::MAX;
            while seen != chunks.load(Ordering::Relaxed) {
                seen = chunks.load(Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(250));
            }
            let _ = writeln!(std::io::stdout(), "{STDERR_WEDGED}");
            begin_winding_with(Duration::from_millis(200));
        }
        other => panic!("unknown case {other}"),
    }
    std::thread::sleep(Duration::from_secs(30));
    let _ = writeln!(std::io::stdout(), "{NEVER_FINISHED}");
}

fn run_case(case: &str) -> Output {
    let out = Command::new(std::env::current_exe().expect("the test binary has a path"))
        .args(["--exact", CHILD_TEST, "--nocapture"])
        .env(CASE_ENV, case)
        .output()
        .expect("the test binary starts");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains(NEVER_FINISHED),
        "{case}: the deadline never ended the exit\n{stdout}"
    );
    out
}

/// Window, menu, tray: no outside source; the deadline ends the hung exit with status 0.
#[test]
fn a_hung_ordinary_exit_ends_at_its_deadline_with_status_zero() {
    let out = run_case("ordinary");
    // Without its marker the child never started the exit: a filter matching nothing would pass.
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(ORDINARY_STARTED),
        "the child never started the exit"
    );
    assert_eq!(out.status.code(), Some(0), "{:?}", out.status);
}

/// The session's last write, behind a stuck actor whose queue is full, holds the end no longer
/// than the flush: the send blocks the flushing thread, not the caller.
#[test]
fn a_last_write_behind_a_stuck_actor_waits_no_longer_than_the_flush() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db: &'static crate::db::DbActor = Box::leak(Box::new(
        crate::db::DbActor::open(dir.path()).expect("db actor"),
    ));
    let (started_tx, started_rx) = mpsc::channel();
    let (release, held) = mpsc::channel::<()>();
    db.post(move |_, _| {
        let _ = started_tx.send(());
        let _ = held.recv();
    });
    started_rx.recv().expect("the actor runs the blocker");
    for _ in 0..crate::db::QUEUE_CAPACITY {
        db.post(|_, _| {});
    }
    // Never released; the actor stays stuck for the rest of the test binary.
    std::mem::forget(release);

    let (returned_tx, returned_rx) = mpsc::channel();
    std::thread::spawn(move || {
        last_words_on(Some(db), None, Some(Box::new(|_, _| {})));
        let _ = returned_tx.send(());
    });
    assert!(
        returned_rx
            .recv_timeout(FLUSH_BOUND + Duration::from_secs(1))
            .is_ok(),
        "the last write held the caller past the flush's bound"
    );
}

#[cfg(unix)]
const STDERR_WEDGED: &str = "STDERR WEDGED";

/// A stderr nobody reads is what a hung exit can be stuck on: the deadline must still end the
/// process, and its line must reach `console.log`, which the wedge cannot block.
#[cfg(unix)]
#[test]
fn a_forced_end_with_a_wedged_stderr_still_ends_and_leaves_its_line() {
    // `logging`'s own test variable, private to its tests: where a test build opens `console.log`.
    const SINK_DIR_ENV: &str = "TIDALUNAR_LOGGING_TEST_DIR";
    const CHILD_DEADLINE: Duration = Duration::from_secs(10);
    let dir = tempfile::tempdir().expect("temp dir");
    let stdout_path = dir.path().join("child.out");
    let mut child = Command::new(std::env::current_exe().expect("the test binary has a path"))
        .args(["--exact", CHILD_TEST, "--nocapture"])
        .env(CASE_ENV, "stderr_wedged")
        .env(SINK_DIR_ENV, dir.path())
        .stdout(std::fs::File::create(&stdout_path).expect("child stdout file"))
        // The child's stderr is never read and stays full while it lives.
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the test binary starts");

    // The wait is for the end itself, which only the deadline's decision brings; a child that
    // never ends is killed, failing the test instead of hanging the run.
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("the child can be polled") {
            break Some(status);
        }
        if started.elapsed() > CHILD_DEADLINE {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
    let log = std::fs::read_to_string(dir.path().join("console.log")).unwrap_or_default();
    // Without its marker the child never reached the wedge: a filter matching nothing would pass.
    assert!(
        stdout.contains(STDERR_WEDGED),
        "the child never wedged its stderr\n{stdout}"
    );
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(0),
        "the forced end did not finish within {CHILD_DEADLINE:?} on a wedged stderr"
    );
    assert!(
        log.contains("did not finish by its deadline"),
        "the forced end left no line in console.log\n{log}"
    );
}

// A link sent once the exit is decided must find the channel shut while the process still runs;
// its sender then waits for the lock and starts with it.

#[cfg(target_os = "linux")]
const CHANNEL_READY: &str = "CHANNEL READY";

/// Runs `case` with a private runtime directory, waits for its marker, then sends one datagram
/// to its channel while it still runs.
#[cfg(target_os = "linux")]
fn send_to_channel_of(case: &str) -> std::io::Result<usize> {
    use std::io::BufRead;
    use std::os::unix::fs::PermissionsExt;
    // Owner-only, as the XDG spec has it: anything wider sends the channel to the fallback.
    let runtime = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .expect("temp dir");
    let mut child = Command::new(std::env::current_exe().expect("the test binary has a path"))
        .args(["--exact", CHILD_TEST, "--nocapture"])
        .env(CASE_ENV, case)
        .env("XDG_RUNTIME_DIR", runtime.path())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("the test binary starts");
    let stdout = child.stdout.take().expect("piped stdout");
    let ready = std::io::BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .any(|line| line.contains(CHANNEL_READY));
    let still_running = matches!(child.try_wait(), Ok(None));
    let socket = std::fs::read_dir(runtime.path())
        .expect("the runtime directory lists")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "sock"));
    let sent = socket
        .ok_or_else(|| std::io::Error::other("no channel in the runtime directory"))
        .and_then(|path| {
            let sender = std::os::unix::net::UnixDatagram::unbound()?;
            sender.connect(path)?;
            sender.send(b"focus")
        });
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        ready && still_running,
        "{case}: the child never held the channel"
    );
    sent
}

#[cfg(target_os = "linux")]
#[test]
fn a_decided_exit_shuts_the_channel_before_the_process_ends() {
    let refused = send_to_channel_of("channel_engaged")
        .expect_err("the channel took a datagram after the exit was decided");
    assert_eq!(refused.kind(), std::io::ErrorKind::BrokenPipe, "{refused}");
}
