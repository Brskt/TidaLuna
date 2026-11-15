//! The exit every Quit takes, and what ends the process when it does not finish.
//!
//! Once it begins a deadline stands: past it, the queued writes get a bounded flush and the
//! process ends with status 0.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use cef::*;

/// How far the process has come. Only moves forward.
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

/// The exit has begun; the page may no longer hold it back.
pub(crate) fn winding() -> bool {
    matches!(phase(), Phase::Winding)
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
            underway: winding(),
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

/// Begins the exit; it must finish within [`GRACE`], or the process ends.
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

/// How the process ends when the normal exit does not finish.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum End {
    /// A quit from within the app (window, menu, tray, updater): status 0, as the exit it
    /// replaces would have left.
    Ordinary,
}

impl End {
    /// Runs on a thread while others may hold locks (the exit this replaces is stuck); no exit
    /// handler runs here, as in Chromium's own forced exits.
    fn now(self) -> ! {
        match self {
            #[cfg(target_os = "windows")]
            End::Ordinary => terminate(0),
            #[cfg(unix)]
            // SAFETY: ends the process; nothing of ours is left to run.
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

/// The exit under way.
struct Pending {
    /// When the watchdog ends the process.
    deadline: Instant,
    /// How it ends then; `None` once the watchdog took it.
    end: Option<End>,
}

/// The watchdog waits on the condvar for the deadline.
static PENDING: (Mutex<Option<Pending>>, Condvar) = (Mutex::new(None), Condvar::new());

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

/// Whether CEF took the task.
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
