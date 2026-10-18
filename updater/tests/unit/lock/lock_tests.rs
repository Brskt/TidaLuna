//! Tests for `src/lock.rs`, attached by `#[path]`. They take the real install
//! lock, and fail rather than pass untested while an install holds it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Threading::CreateEventW;

use super::*;

/// What the second taker must find the lock to be, set in the child the test
/// launches.
const SECOND_TAKER: &str = "TIDALUNAR_TEST_SECOND_TAKER";
const HELD: &str = "held";
const FREE: &str = "free";

/// The part a peer plays, and the legacy name it plays it on.
const PEER_ROLE: &str = "TIDALUNAR_TEST_PEER_ROLE";
const PEER_LEGACY_NAME: &str = "TIDALUNAR_TEST_PEER_LEGACY_NAME";
/// Printed by a peer once it holds its handle, before it waits for the parent.
const PEER_READY: &str = "peer-ready";
/// A peer reaches `PEER_READY` through a process start and kernel calls that
/// never block. One that has not printed it by then is stuck, not slow.
const PEER_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// A process can open the namespace only once, and the namespace mutex has one
/// name: the tests that take the lock run one at a time.
static LOCK_TESTS: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    LOCK_TESTS.lock().unwrap_or_else(|e| e.into_inner())
}

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

    let _serial = serial();
    let held = try_acquire()
        .expect("the lock is reachable")
        .expect("no installer or updater of this account is running");
    second_taker_finds(HELD);
    drop(held);
    second_taker_finds(FREE);
}

/// A legacy prefix no earlier build uses, unique to this test run.
fn test_prefix() -> String {
    format!("Global\\TidaLunarInstallLockTest-{}-", std::process::id())
}

/// The legacy name `acquire(prefix)` builds.
fn legacy_name(prefix: &str) -> String {
    let user = current_user().expect("the current user");
    format!("{prefix}{}", user.sid_text)
}

/// A second process: the test binary rerun on `test` alone, playing `role`. A
/// peer that holds a handle waits for `finish`, which closes its stdin; every
/// wait in a peer is that pipe or a zero-timeout wait.
struct Peer {
    child: Child,
    /// The peer's stdout, line by line, closed at its end. A std pipe has no
    /// read timeout: a thread reads it, and `ready` bounds its wait on this.
    lines: Receiver<String>,
    seen: String,
}

impl Peer {
    fn spawn(test: &str, role: &str, legacy_name: &str) -> Self {
        let module = module_path!()
            .split_once("::")
            .map_or(module_path!(), |(_, rest)| rest);
        let mut child =
            std::process::Command::new(std::env::current_exe().expect("the test binary"))
                .args([
                    &format!("{module}::{test}"),
                    "--exact",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(PEER_ROLE, role)
                .env(PEER_LEGACY_NAME, legacy_name)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("launch the peer");
        let stdout = BufReader::new(child.stdout.take().expect("the peer's stdout"));
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in stdout.lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            seen: String::new(),
        }
    }

    /// Returns once the peer has printed `PEER_READY`; panics if it exits first.
    /// Matched at the line's end: with one test thread, libtest writes
    /// `test <name> ... ` before the test body and ends that line only with the
    /// result.
    fn ready(mut self) -> Self {
        let deadline = Instant::now() + PEER_READY_TIMEOUT;
        loop {
            match self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(line) => {
                    self.seen.push_str(&line);
                    self.seen.push('\n');
                    if line.trim_end().ends_with(PEER_READY) {
                        return self;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("the peer exited before it was ready:\n{}", self.seen)
                }
                Err(RecvTimeoutError::Timeout) => {
                    // Killed, or it would hold its handle until this binary exits.
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    panic!(
                        "the peer was not ready after {PEER_READY_TIMEOUT:?}:\n{}",
                        self.seen
                    )
                }
            }
        }
    }

    /// Lets the peer go on, and requires it to run its test to a pass.
    fn finish(mut self) {
        let mut stdin = self.child.stdin.take().expect("the peer's stdin");
        // A peer that already exited closed the pipe; its output says why.
        let _ = writeln!(stdin, "go");
        drop(stdin);
        for line in self.lines.iter() {
            self.seen.push_str(&line);
            self.seen.push('\n');
        }
        let status = self.child.wait().expect("wait for the peer");
        assert!(
            status.success() && self.seen.contains("1 passed"),
            "the peer did not run to a pass ({status}):\n{}",
            self.seen
        );
    }
}

/// Blocks until the parent writes a line or exits.
fn await_parent() {
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

/// The peer's role, with the legacy name it plays it on; `None` in the parent.
fn peer_role() -> Option<(String, Vec<u16>)> {
    let role = std::env::var(PEER_ROLE).ok()?;
    let name = std::env::var(PEER_LEGACY_NAME).expect("the legacy name");
    Some((role, wide(&name)))
}

const LEGACY_HOLDER: &str = "legacy-holder";
const NAMESPACE_WITNESS: &str = "namespace-witness";
const LEGACY_TAKER_HELD: &str = "legacy-taker-held";
const LEGACY_TAKER_FREE: &str = "legacy-taker-free";

/// The earlier builds' acquisition: a NULL-SA create, then a zero wait.
fn take_legacy_as_earlier_build(name: &[u16]) -> (HANDLE, u32) {
    unsafe {
        let mutex = CreateMutexW(ptr::null(), FALSE, name.as_ptr());
        assert!(
            !mutex.is_null(),
            "create the legacy name: {}",
            GetLastError()
        );
        (mutex, WaitForSingleObject(mutex, 0))
    }
}

#[expect(
    clippy::print_stdout,
    reason = "the parent reads PEER_READY from the peer's stdout, with --nocapture"
)]
fn play_peer(role: &str, legacy: &[u16]) {
    match role {
        LEGACY_HOLDER => {
            let (_mutex, wait) = take_legacy_as_earlier_build(legacy);
            assert!(wait == WAIT_OBJECT_0, "the legacy name was taken: {wait}");
            println!("{PEER_READY}");
            await_parent();
        }
        NAMESPACE_WITNESS => {
            // A handle on the namespace mutex across the parent's refusal keeps
            // the object alive: only a release lets this wait succeed.
            let user = current_user().expect("the current user");
            let namespace = UserNamespace::open(&user).expect("open the install namespace");
            let attributes = attributes_of(namespace.descriptor);
            let name = wide(&format!("{NAMESPACE}\\{MUTEX_LEAF}"));
            let mutex = unsafe { CreateMutexW(&attributes, FALSE, name.as_ptr()) };
            assert!(!mutex.is_null(), "open the namespace mutex");
            println!("{PEER_READY}");
            await_parent();
            let wait = unsafe { WaitForSingleObject(mutex, 0) };
            assert!(
                wait == WAIT_OBJECT_0,
                "the refusing process kept the namespace mutex: {wait}"
            );
            unsafe {
                ReleaseMutex(mutex);
                CloseHandle(mutex);
            }
        }
        LEGACY_TAKER_HELD => {
            let (mutex, wait) = take_legacy_as_earlier_build(legacy);
            unsafe { CloseHandle(mutex) };
            assert!(
                wait == WAIT_TIMEOUT,
                "an earlier build took the legacy name beside us: {wait}"
            );
        }
        LEGACY_TAKER_FREE => {
            let (mutex, wait) = take_legacy_as_earlier_build(legacy);
            unsafe { CloseHandle(mutex) };
            assert!(
                wait == WAIT_OBJECT_0,
                "the legacy name stayed taken after release: {wait}"
            );
        }
        other => panic!("unknown role {other:?}"),
    }
}

#[test]
fn an_earlier_builds_hold_refuses_and_releases_the_namespace_lock() {
    const TEST: &str = "an_earlier_builds_hold_refuses_and_releases_the_namespace_lock";
    if let Some((role, legacy)) = peer_role() {
        return play_peer(&role, &legacy);
    }

    let _serial = serial();
    let prefix = test_prefix();
    let name = legacy_name(&prefix);
    let holder = Peer::spawn(TEST, LEGACY_HOLDER, &name).ready();
    let witness = Peer::spawn(TEST, NAMESPACE_WITNESS, &name).ready();
    let got = acquire(&prefix).expect("the lock is reachable");
    assert!(
        got.is_none(),
        "took the lock beside an earlier build's hold"
    );
    // The refusing process is still alive: only its release, not its exit, can
    // let the witness's wait succeed.
    witness.finish();
    holder.finish();
}

#[test]
fn an_earlier_build_finds_the_legacy_name_held() {
    const TEST: &str = "an_earlier_build_finds_the_legacy_name_held";
    if let Some((role, legacy)) = peer_role() {
        return play_peer(&role, &legacy);
    }

    let _serial = serial();
    let prefix = test_prefix();
    let name = legacy_name(&prefix);
    let held = acquire(&prefix)
        .expect("the lock is reachable")
        .expect("no installer or updater of this account is running");
    assert!(!held.legacy.is_null(), "the legacy name was not taken");
    Peer::spawn(TEST, LEGACY_TAKER_HELD, &name).finish();
    drop(held);
    Peer::spawn(TEST, LEGACY_TAKER_FREE, &name).finish();
}

#[test]
fn a_legacy_name_on_another_object_type_does_not_block() {
    let _serial = serial();
    let prefix = test_prefix();
    let name = wide(&legacy_name(&prefix));
    let event = unsafe { CreateEventW(ptr::null(), FALSE, FALSE, name.as_ptr()) };
    assert!(!event.is_null(), "create the squatting event");
    let got = acquire(&prefix).expect("the lock is reachable");
    unsafe { CloseHandle(event) };
    let held = got.expect("a squatted legacy name blocked the lock");
    assert!(
        held.legacy.is_null(),
        "took a legacy handle on another object type"
    );
}
