//! Quit requests from outside the window: a signal, a console event, `terminate:`, the session
//! ending.
//!
//! Each one asks for the exit the window's own close takes, so that `main` returns and the
//! stores close. When that exit cannot start, or has not finished by its deadline, the queued
//! writes get a bounded flush and the process ends the way the request would have ended it.

#[cfg(target_os = "windows")]
pub(crate) mod console;
#[cfg(unix)]
pub(crate) mod signal;

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use cef::*;

/// How far the process has come, as an outside quit request sees it. Only moves forward.
#[repr(u8)]
pub(crate) enum Phase {
    /// No message loop takes tasks yet; a posted exit would never run.
    Boot = 0,
    /// The exit goes through the window once the loop takes tasks.
    Running = 1,
    /// The exit has begun, whatever asked for it.
    Winding = 2,
}

static PHASE: AtomicU8 = AtomicU8::new(Phase::Boot as u8);

/// Set by the first outside request, in the signal handler itself on Unix, and never cleared.
static REQUESTED: AtomicBool = AtomicBool::new(false);

/// What the normal exit gets before the process ends without it, when the OS sets no deadline:
/// Chrome's bound on the same path (`EndSession`, its macOS SIGTERM watchdog).
pub(crate) const GRACE: Duration = Duration::from_secs(10);

/// What the queued writes get when the normal exit did not run (the actor may be what hangs).
const FLUSH_BOUND: Duration = Duration::from_millis(1500);

pub(crate) fn enter(phase: Phase) {
    PHASE.fetch_max(phase as u8, Ordering::AcqRel);
}

fn phase() -> Phase {
    match PHASE.load(Ordering::Acquire) {
        0 => Phase::Boot,
        1 => Phase::Running,
        _ => Phase::Winding,
    }
}

/// An outside quit request came in. Never cleared.
pub(crate) fn requested() -> bool {
    REQUESTED.load(Ordering::Acquire)
}

/// The exit has begun; the page may no longer hold it back.
pub(crate) fn winding() -> bool {
    matches!(phase(), Phase::Winding)
}

/// A request came in or the exit began. Never cleared. An exit cannot be called off.
pub(crate) fn exiting() -> bool {
    requested() || winding()
}

/// Whether the exit had begun when a decision was taken. A decision that must not blame anything
/// on the exit takes one, and outside tests [`Exit::now`] is the only way to make one.
#[derive(Clone, Copy)]
pub(crate) struct Exit {
    underway: bool,
}

impl Exit {
    pub(crate) fn now() -> Exit {
        Exit {
            underway: exiting(),
        }
    }

    pub(crate) fn is_underway(self) -> bool {
        self.underway
    }

    #[cfg(test)]
    pub(crate) fn underway() -> Exit {
        Exit { underway: true }
    }

    #[cfg(test)]
    pub(crate) fn not_yet() -> Exit {
        Exit { underway: false }
    }
}

/// Begins the exit, whatever asked for it; it must finish within [`GRACE`], or the process
/// ends. An outside request's deadline and end stand.
pub(crate) fn begin_winding() {
    begin_winding_with(GRACE);
}

fn begin_winding_with(grace: Duration) {
    crate::platform::app_lock::shut_channel();
    enter(Phase::Winding);
    let deadline = Instant::now() + grace;
    let mut pending = PENDING.0.lock().unwrap_or_else(|e| e.into_inner());
    if pending.is_none() {
        *pending = Some(Pending {
            seen: vec![End::Ordinary],
            deadline,
            end: Some(End::Ordinary),
        });
        drop(pending);
        spawn_watchdog();
    }
}

/// How long the runtime's blocking tasks may hold up the end of `main`: until the exit's
/// deadline, less what the stores closing after them need ([`FLUSH_BOUND`], as for a flush).
/// With no exit under way (an early return, an unwind), [`GRACE`].
pub(crate) fn runtime_bound() -> Duration {
    let pending = PENDING.0.lock().unwrap_or_else(|e| e.into_inner());
    pending.as_ref().map_or(GRACE, |p| {
        p.deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(FLUSH_BOUND)
    })
}

/// Where a request comes from, and how the process ends when the normal exit does not: as the
/// request would have ended it. Equal values are one source asking again.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum End {
    /// The signal raised again with its default action.
    #[cfg(unix)]
    Signal(libc::c_int),
    /// Ctrl+C or Ctrl+Break, both the user at the console. Like the console closing, it ends with
    /// `STATUS_CONTROL_C_EXIT`, the code Windows' own Ctrl+C handling leaves.
    #[cfg(target_os = "windows")]
    ConsoleInterrupt,
    #[cfg(target_os = "windows")]
    ConsoleClose,
    /// What AppKit's own `terminate:` ends in, an exit with status 0.
    #[cfg(target_os = "macos")]
    Terminate,
    /// A quit from within the app (window, menu, tray, updater): status 0, as the exit it
    /// replaces would have left.
    Ordinary,
}

impl End {
    /// Only Ctrl+C or Ctrl+Break pressed again means "now". The kernel ends a repeated SIGINT
    /// or SIGTERM itself (one-shot handler); two SIGHUPs are one terminal closing, and AppKit
    /// sends `terminate:` again for every Quit, which Chromium ignores too.
    fn repeat_ends_now(self) -> bool {
        match self {
            #[cfg(unix)]
            End::Signal(_) => false,
            #[cfg(target_os = "windows")]
            End::ConsoleInterrupt => true,
            #[cfg(target_os = "windows")]
            End::ConsoleClose => false,
            #[cfg(target_os = "macos")]
            End::Terminate => false,
            End::Ordinary => false,
        }
    }

    /// Runs on a thread while others may hold locks (the exit this replaces is stuck); no exit
    /// handler runs here, as in Chromium's own forced exits.
    fn now(self) -> ! {
        match self {
            #[cfg(unix)]
            End::Signal(sig) => {
                // SAFETY: Chromium's `ReraiseSignalAndExit`. The signal may still reach a handler
                // (SIGHUP keeps ours; `initialize` or our second install may have set one) or be
                // blocked in this thread: both are undone just before raising.
                unsafe {
                    let mut default: libc::sigaction = std::mem::zeroed();
                    default.sa_sigaction = libc::SIG_DFL;
                    libc::sigaction(sig, &default, std::ptr::null_mut());
                    let mut set: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut set);
                    libc::sigaddset(&mut set, sig);
                    libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
                    libc::raise(sig);
                    // Reached only if a handler was set again after the reset above.
                    libc::_exit(128 + sig)
                }
            }
            #[cfg(target_os = "windows")]
            End::ConsoleInterrupt | End::ConsoleClose => {
                terminate(windows_sys::Win32::Foundation::STATUS_CONTROL_C_EXIT as u32)
            }
            #[cfg(target_os = "windows")]
            End::Ordinary => terminate(0),
            #[cfg(target_os = "macos")]
            // SAFETY: ends the process; nothing of ours is left to run.
            End::Terminate => unsafe { libc::_exit(0) },
            #[cfg(unix)]
            // SAFETY: as above.
            End::Ordinary => unsafe { libc::_exit(0) },
        }
    }
}

/// Not `ExitProcess`, whose DLL detach would run into CEF's threads mid-exit; Chrome ends this
/// way too.
#[cfg(target_os = "windows")]
fn terminate(code: u32) -> ! {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
    // SAFETY: the pseudo-handle of this process.
    unsafe { TerminateProcess(GetCurrentProcess(), code) };
    std::process::abort()
}

/// What a forced end still does, off the ending thread: `line`, the unshown link, `last` and
/// the queued writes. The ending thread waits [`FLUSH_BOUND`] at most, whatever hangs (stderr, a
/// lock); lines and writes each get a thread, neither costing the other.
fn last_words(line: Option<&'static str>, last: Option<crate::db::BoxedFn>) {
    last_words_on(crate::state::DB.get(), line, last);
}

fn last_words_on(
    db: Option<&'static crate::db::DbActor>,
    line: Option<&'static str>,
    last: Option<crate::db::BoxedFn>,
) {
    let deadline = Instant::now() + FLUSH_BOUND;
    // A helper that panics counts as done, the ending thread keeping no sender.
    let (done, finished) = mpsc::channel::<()>();
    let flushing = spawn_flush(db, last, done.clone());
    let unflushed = flushing.as_ref().err().map(ToString::to_string);
    let logging = std::thread::Builder::new()
        .name("quit-log".into())
        .spawn(move || {
            if let Some(line) = line {
                crate::logging::vlog_forced_end(format_args!("{line}"));
            }
            if let Some(e) = unflushed {
                crate::verr!("[QUIT]   Queued writes left unflushed: no thread for them ({e})");
            }
            crate::ui::deep_link::report_unshown();
            let _ = done.send(());
        });
    let helpers = usize::from(flushing.is_ok()) + usize::from(logging.is_ok());
    for _ in 0..helpers {
        let left = deadline.saturating_duration_since(Instant::now());
        if finished.recv_timeout(left).is_err() {
            break;
        }
    }
}

/// Posts `last` then flushes the queues on a thread of its own, which reports on `done`: behind a
/// stuck actor a full queue blocks the post too, and whoever waits must not block on it.
fn spawn_flush(
    db: Option<&'static crate::db::DbActor>,
    last: Option<crate::db::BoxedFn>,
    done: mpsc::Sender<()>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("quit-flush".into())
        .spawn(move || {
            if let Some(db) = db {
                if let Some(write) = last {
                    db.post(write);
                }
                db.flush();
            }
            crate::platform::secure_store::flush();
            let _ = done.send(());
        })
}

/// The exit under way, once a request or the app started it.
struct Pending {
    /// Requests heard so far, which tell a repeat from a new source.
    seen: Vec<End>,
    /// When the watchdog ends the process. Only ever moves earlier.
    deadline: Instant,
    /// How it ends then: as the outside request with the earliest deadline would have, or
    /// [`End::Ordinary`] when none came. `None` once the watchdog took it, or when the exit
    /// could not start and the request ends the process.
    end: Option<End>,
}

/// The condvar wakes the watchdog when a request moves the deadline earlier.
static PENDING: (Mutex<Option<Pending>>, Condvar) = (Mutex::new(None), Condvar::new());

/// Asks for the normal exit, which must finish within `grace` or by an earlier deadline, else the
/// process ends as the outside request with the earliest deadline would have. A repeat from one
/// source changes nothing, except Ctrl+C or Ctrl+Break, which ends it at once.
pub(crate) fn request(end: End, grace: Duration) {
    REQUESTED.store(true, Ordering::Release);
    crate::platform::app_lock::shut_channel();
    let deadline = Instant::now() + grace;
    let (lock, deadline_moved) = &PENDING;
    let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = pending.as_mut() {
        if p.seen.contains(&end) {
            drop(pending);
            if end.repeat_ends_now() {
                last_words(None, None);
                end.now();
            }
        } else {
            p.seen.push(end);
            let earlier = deadline < p.deadline;
            if earlier {
                p.deadline = deadline;
                deadline_moved.notify_all();
            }
            // An outside source says how the process ends, over a quit from within the app.
            if earlier || matches!(p.end, Some(End::Ordinary)) {
                p.end = Some(end);
            }
        }
        return;
    }
    // Held across `post_task` without risk: CEF only queues the task, even from the UI thread
    // (`terminate:`); the exit it runs takes this lock again only once it is released.
    let started = match phase() {
        Phase::Boot => false,
        Phase::Winding => true,
        Phase::Running => post_quit_task(),
    };
    // Set before the flush, keeping a racing request from ending the process twice.
    *pending = Some(Pending {
        seen: vec![end],
        deadline,
        end: started.then_some(end),
    });
    drop(pending);
    if !started {
        last_words(None, None);
        end.now();
    }
    spawn_watchdog();
}

fn spawn_watchdog() {
    let watchdog = std::thread::Builder::new()
        .name("quit-watchdog".into())
        .spawn(|| {
            let (lock, deadline_moved) = &PENDING;
            let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
            let end = loop {
                let p = pending.as_mut().expect("set before the watchdog starts");
                let left = p.deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break p
                        .end
                        .take()
                        .expect("only the watchdog takes the end it waits for");
                }
                pending = deadline_moved
                    .wait_timeout(pending, left)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            };
            drop(pending);
            last_words(
                Some("[QUIT]   The exit did not finish by its deadline; ending the process"),
                None,
            );
            end.now();
        });
    if let Err(e) = watchdog {
        crate::verr!("[QUIT]   No watchdog for the exit ({e})");
    }
}

/// Waits until the writes queued so far reach the disk, or [`FLUSH_BOUND`] passes.
pub(crate) fn flush_bounded() {
    let (done, finished) = mpsc::channel();
    match spawn_flush(crate::state::DB.get(), None, done) {
        Err(e) => crate::verr!("[QUIT]   Queued writes left unflushed: no thread for them ({e})"),
        Ok(_) if finished.recv_timeout(FLUSH_BOUND).is_err() => {
            crate::verr!("[QUIT]   Queued writes did not finish within {FLUSH_BOUND:?}");
        }
        Ok(_) => {}
    }
}

/// The exit every Quit takes; `state.force_quit` makes a close-to-tray window close for real.
/// UI thread only.
pub(crate) fn force_quit() {
    begin_winding();
    crate::app_state::with_state(|state| {
        state.force_quit = true;
    });
    if let Some(window) = crate::ui::app_window::AppWindow::current() {
        window.close();
    } else {
        crate::app_state::quit_loop();
    }
}

/// [`force_quit`] from any thread. CEF refuses the task only once it has shut down, which the
/// exit reaches past the window: nothing is left to start then.
pub(crate) fn post_force_quit() {
    begin_winding();
    if !post_quit_task() {
        crate::vprintln!("[QUIT]   Quit asked after CEF shut down; the exit is already under way");
    }
}

/// Holds no lock: [`request`] calls it under [`PENDING`]'s.
fn post_quit_task() -> bool {
    let mut task = QuitTask::new(0);
    post_task(ThreadId::UI, Some(&mut task)) == 1
}

wrap_task! {
    struct QuitTask {
        _p: u8,
    }
    impl Task {
        fn execute(&self) {
            force_quit();
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/platform/quit/ordinary.rs"]
mod tests;
