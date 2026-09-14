use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use time::{OffsetDateTime, UtcOffset};

/// Maximum supported log level; values are clamped to `0..=MAX_LOG_LEVEL`.
pub(crate) const MAX_LOG_LEVEL: u8 = 3;

/// The `LOGS` env var, parsed once. Acts as a dev floor for the effective level.
static ENV_LOG_LEVEL: LazyLock<u8> = LazyLock::new(|| {
    std::env::var("LOGS")
        .ok()
        .and_then(|v| v.parse::<u8>().ok())
        .filter(|&level| level <= MAX_LOG_LEVEL)
        .unwrap_or(0)
});

/// The live, runtime-mutable effective level. Initialised from the env floor at
/// the very top of `main`, then raised to `max(env, persisted)` once the DB is up.
static LOG_LEVEL: AtomicU8 = AtomicU8::new(0);

/// The `console.log` append handle. Opened lazily: when the level first reaches >= 1, or on the
/// first `verr!`, which writes whatever the level says and opens the sink at level 0 too.
static FILE_SINK: Mutex<Option<File>> = Mutex::new(None);

#[inline]
pub fn log_level() -> u8 {
    LOG_LEVEL.load(Ordering::Relaxed)
}

/// The `LOGS` env floor (read-once). Used by the Windows console sequencing in
/// `main`; the console *window* is Windows-only, as is this accessor.
#[cfg(target_os = "windows")]
pub(crate) fn env_log_level() -> u8 {
    *ENV_LOG_LEVEL
}

fn store_level(level: u8) {
    LOG_LEVEL.store(level, Ordering::Relaxed);
}

/// Seed the live level from the env floor before anything logs (top of `main`).
pub(crate) fn init_env_floor() {
    store_level(*ENV_LOG_LEVEL);
}

/// Apply a requested level live: clamp, raise to `max(env, requested)`, store, and
/// open the file sink if logging is now active. Used both at boot (with the
/// persisted value) and from the `settings.set_log_level` IPC handler.
pub(crate) fn set_log_level(requested: u8) {
    let level = effective_level(*ENV_LOG_LEVEL, requested.min(MAX_LOG_LEVEL));
    store_level(level);
    if level >= 1 {
        ensure_file_sink();
    }
}

/// Open `<data_dir>/console.log` for append if not already open. Idempotent.
fn ensure_file_sink() {
    let mut guard = FILE_SINK.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return;
    }
    // Create the data dir first: on a fresh profile this runs before main's own
    // create_dir_all, and OpenOptions won't create missing parents.
    let dir = crate::state::cache_data_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("console.log"))
    {
        *guard = Some(file);
    }
}

/// Archive the previous session's `console.log`, for the process that owns the
/// session and only for it.
///
/// The lock is the precondition, not decoration. Every process of this app
/// shares one data dir, so a CEF subprocess or a duplicate launch rotating here
/// would rename the file the surviving browser holds open, which succeeds
/// silently on Linux and on Windows alike; the survivor would go on writing
/// into the archive while `console.log` no longer existed. Neither of those two
/// ever holds the lock. A subprocess returns from `execute_process` and a
/// duplicate from the guard, both before it is taken.
///
/// Nothing may open the sink before this runs, an open sink being exactly what
/// makes the rotation stand down. A filled slot is therefore a caller out of
/// order and never a runtime condition, which is why startup keeps its verdicts
/// until here, `AppLock::report_link_channel` among them.
pub(crate) fn adopt_session_log(_lock: &crate::platform::app_lock::AppLock) {
    let mut guard = FILE_SINK.lock().unwrap_or_else(|e| e.into_inner());
    debug_assert!(
        guard.is_none(),
        "a diagnostic opened the log before the session could rotate it"
    );
    adopt_leftover(&crate::state::cache_data_dir(), &mut guard);
}

/// The half that does not need the lock to be checked, an empty slot proving
/// this process holds no handle on the file about to be renamed. Held across the
/// rename so a `verr!` on another thread cannot open one midway.
fn adopt_leftover(dir: &Path, sink: &mut Option<File>) {
    if sink.is_some() {
        return;
    }
    rotate_console_log(dir);
}

#[inline]
pub fn vlog(args: std::fmt::Arguments<'_>) {
    if log_level() < 1 {
        return;
    }
    print_log(args);
}

#[inline]
pub fn vlog2(args: std::fmt::Arguments<'_>) {
    if log_level() < 2 {
        return;
    }
    print_log(args);
}

#[inline]
pub fn vlog3(args: std::fmt::Arguments<'_>) {
    if log_level() < 3 {
        return;
    }
    print_log(args);
}

/// Burst and window for the ungated path, taken from the Linux kernel's own `___ratelimit`
/// defaults (`DEFAULT_RATELIMIT_BURST`, `DEFAULT_RATELIMIT_INTERVAL`) rather than chosen here.
const ERR_BURST: u32 = 10;
const ERR_INTERVAL_MS: u64 = 5_000;

/// The monotonic origin the budget measures against. A wall clock would let a system-time change
/// re-arm the burst or freeze it.
static PROCESS_START: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);

/// The window's start and the budget left move together in one word. Split across two atomics
/// they could not: two callers reading a spent budget both subtracted and wrapped the count past
/// `u32::MAX`, and two finding the window expired both granted a write. Budget in the low 16 bits.
const ERR_LEFT_BITS: u32 = 16;
const ERR_LEFT_MASK: u64 = (1 << ERR_LEFT_BITS) - 1;
const ERR_WINDOW_MASK: u64 = u64::MAX >> ERR_LEFT_BITS;
const _: () = assert!(ERR_BURST as u64 <= ERR_LEFT_MASK);

static ERR_STATE: AtomicU64 = AtomicU64::new(ERR_BURST as u64);
static ERR_MISSED: AtomicU32 = AtomicU32::new(0);

/// What the ungated path owes one caller. Three outcomes, so a named enum: the report is a write
/// as well, but one that has to carry what the silence cost.
enum ErrBudget {
    Write,
    /// First write past a window that had been spent, naming the lines it swallowed.
    WroteAfterMissing(u32),
    Drop,
}

/// The rule, kept free of IO so the budget can be exercised without a sink. Nothing but the
/// flood's own next call past a spent window ever reports what it dropped, which is what makes a
/// timer unnecessary.
fn err_budget(now_ms: u64, state: &AtomicU64, missed: &AtomicU32) -> ErrBudget {
    // Masking keeps a saturated clock from shifting into the budget's own bits.
    let now_ms = now_ms & ERR_WINDOW_MASK;
    // One compare-and-swap settles both halves: roll the window, or claim a unit from the one
    // standing. `fetch_update` retries until it wins: whoever succeeds is the only caller to
    // have acted on the value it read, which a load followed by a separate subtract cannot promise.
    let claimed = state.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |packed| {
        if now_ms.saturating_sub(packed >> ERR_LEFT_BITS) >= ERR_INTERVAL_MS {
            return Some((now_ms << ERR_LEFT_BITS) | u64::from(ERR_BURST - 1));
        }
        // A plain decrement never borrows from the window, because the budget sits in the low bits.
        if packed & ERR_LEFT_MASK > 0 {
            return Some(packed - 1);
        }
        None
    });
    match claimed {
        // The word this caller replaced says which of the two it did. Reading the pre-image beats
        // a flag set inside the closure, which every retry would have to leave in step.
        Ok(previous) if now_ms.saturating_sub(previous >> ERR_LEFT_BITS) >= ERR_INTERVAL_MS => {
            ErrBudget::WroteAfterMissing(missed.swap(0, Ordering::Relaxed))
        }
        Ok(_) => ErrBudget::Write,
        Err(_) => {
            missed.fetch_add(1, Ordering::Relaxed);
            ErrBudget::Drop
        }
    }
}

/// A hard failure with no other user-facing channel: never gated by `LOGS`, and it opens the sink
/// itself, so the trace survives at level 0. Being ungated is also what lets an untrusted caller
/// drive the disk at any level, and `console.log` rotates only at launch. The budget lives here
/// because three call sites had hand-rolled a guard and the fourth forgot it.
pub fn vlog_err(args: std::fmt::Arguments<'_>) {
    let now_ms = u64::try_from(PROCESS_START.elapsed().as_millis()).unwrap_or(u64::MAX);
    let missed = match err_budget(now_ms, &ERR_STATE, &ERR_MISSED) {
        ErrBudget::Drop => return,
        ErrBudget::Write => 0,
        ErrBudget::WroteAfterMissing(missed) => missed,
    };
    ensure_file_sink();
    if missed > 0 {
        print_log(format_args!(
            "[LOG]    {missed} hard failures went unwritten while the budget was spent"
        ));
    }
    print_log(args);
}

/// One timestamped line, its newline included.
///
/// The newline belongs to the buffer, not to the call that writes it. Several
/// processes of this app share one `console.log`, and `O_APPEND` makes each
/// WRITE atomic, not each line: emitting the text and the newline separately
/// interleaved four fifths of the lines under four concurrent writers.
fn compose_line(now: OffsetDateTime, args: std::fmt::Arguments<'_>) -> String {
    format!(
        "[{:02}:{:02}:{:02}:{:03}] {}\n",
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond(),
        args
    )
}

// The crate's only writer: the deny targets its callers, not the sink itself.
#[allow(clippy::print_stderr)]
#[inline]
fn print_log(args: std::fmt::Arguments<'_>) {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let line = compose_line(now, args);
    eprint!("{line}");
    // Callers gate themselves: vlog* past their level, vlog_err by opening the sink.
    let mut guard = FILE_SINK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(file) = guard.as_mut() {
        let _ = file.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! vprintln {
    ($($arg:tt)*) => {
        $crate::logging::vlog(format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! vprintln2 {
    ($($arg:tt)*) => {
        $crate::logging::vlog2(format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! vprintln3 {
    ($($arg:tt)*) => {
        $crate::logging::vlog3(format_args!($($arg)*))
    };
}

/// Ungated: the level decides how chatty the log is, not whether a failure is traced.
#[macro_export]
macro_rules! verr {
    ($($arg:tt)*) => {
        $crate::logging::vlog_err(format_args!($($arg)*))
    };
}

/// Effective level = the louder of the `LOGS` env floor and the persisted setting.
pub(crate) fn effective_level(env: u8, setting: u8) -> u8 {
    env.max(setting)
}

/// Archive filename for a rotated session log, e.g.
/// `console-2026-06-07_14-30-05-000.log`. Millisecond precision keeps two
/// same-second rotations from colliding (the `rename` would overwrite) while the
/// fixed-width fields keep lexicographic order == chronological for `prune_logs`.
fn archive_name(ts: OffsetDateTime) -> String {
    format!(
        "console-{:04}-{:02}-{:02}_{:02}-{:02}-{:02}-{:03}.log",
        ts.year(),
        u8::from(ts.month()),
        ts.day(),
        ts.hour(),
        ts.minute(),
        ts.second(),
        ts.millisecond(),
    )
}

/// Keep only the `keep` newest `console-*.log` archives in `dir`; delete the rest.
/// Names embed a `YYYY-MM-DD_HH-MM-SS` stamp; lexicographic order == chronological.
fn prune_logs(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("console-") && n.ends_with(".log"))
        })
        .collect();
    files.sort();
    if files.len() > keep {
        for old in &files[..files.len() - keep] {
            let _ = std::fs::remove_file(old);
        }
    }
}

/// Best-effort local offset; falls back to UTC (mirrors `print_log`'s guard,
/// since `current_local_offset` can fail off the main thread).
fn local_offset() -> UtcOffset {
    UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC)
}

/// Archive a leftover `console.log` into `<data_dir>/logs/` named by its
/// last-modified time, then prune to the 20 most recent. `console.log` always
/// means "this session"; `logs/` holds past sessions.
///
/// Reached only through `adopt_session_log`, which owns both preconditions: who
/// may rotate, and that nothing here holds the file yet. Unconditional for the
/// owning launch, archiving a leftover even when this session keeps logging
/// off.
fn rotate_console_log(data_dir: &Path) {
    let current = data_dir.join("console.log");
    let Ok(meta) = std::fs::metadata(&current) else {
        return; // nothing to rotate
    };
    let ts = meta
        .modified()
        .ok()
        .map(|m| OffsetDateTime::from(m).to_offset(local_offset()))
        .unwrap_or_else(|| {
            OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc())
        });

    let logs_dir = data_dir.join("logs");
    if std::fs::create_dir_all(&logs_dir).is_err() {
        return;
    }
    let _ = std::fs::rename(&current, logs_dir.join(archive_name(ts)));
    prune_logs(&logs_dir, 20);
}

#[cfg(test)]
#[path = "../tests/unit/logging.rs"]
mod tests;
