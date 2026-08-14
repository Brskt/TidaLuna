use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
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
pub(crate) fn ensure_file_sink() {
    let mut guard = FILE_SINK.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return;
    }
    // Create the data dir first: on a fresh profile this runs before main's own
    // create_dir_all, and OpenOptions won't create missing parents.
    let dir = crate::state::cache_data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("console.log");
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        *guard = Some(file);
    }
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

/// A hard failure that has no other user-facing channel: never gated by `LOGS`,
/// and it opens the sink itself: the trace survives at level 0.
pub fn vlog_err(args: std::fmt::Arguments<'_>) {
    ensure_file_sink();
    print_log(args);
}

// The crate's only writer: the deny targets its callers, not the sink itself.
#[allow(clippy::print_stderr)]
#[inline]
fn print_log(args: std::fmt::Arguments<'_>) {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let line = format!(
        "[{:02}:{:02}:{:02}:{:03}] {}",
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond(),
        args
    );
    eprintln!("{line}");
    // Callers gate themselves: vlog* past their level, vlog_err by opening the sink.
    let mut guard = FILE_SINK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(file) = guard.as_mut() {
        let _ = writeln!(file, "{line}");
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

/// At startup, archive a leftover `console.log` into `<data_dir>/logs/` named by
/// its last-modified time, then prune to the 20 most recent. Unconditional:
/// runs even when this session keeps logging off, never losing a leftover.
/// `console.log` always means "this session"; `logs/` holds past sessions.
pub(crate) fn rotate_console_log(data_dir: &Path) {
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
