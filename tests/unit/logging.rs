//! Tests for `src/logging.rs`, attached to it by `#[path]`.

use super::*;
use std::fs;
use time::{Date, Month, PrimitiveDateTime, Time};

static LOGGING_STATE: Mutex<()> = Mutex::new(());

/// libtest runs the crate in one process, several threads wide: a test that
/// mutates LOG_LEVEL or FILE_SINK races its neighbours and dirties the rest of
/// the run. Take this first; it serialises and restores.
struct LoggingState {
    _lock: std::sync::MutexGuard<'static, ()>,
    level: u8,
    sink: Option<File>,
}

impl LoggingState {
    fn acquire() -> Self {
        let _lock = LOGGING_STATE.lock().unwrap_or_else(|e| e.into_inner());
        Self {
            _lock,
            level: log_level(),
            sink: FILE_SINK.lock().unwrap_or_else(|e| e.into_inner()).take(),
        }
    }
}

impl Drop for LoggingState {
    fn drop(&mut self) {
        store_level(self.level);
        *FILE_SINK.lock().unwrap_or_else(|e| e.into_inner()) = self.sink.take();
    }
}

#[test]
fn hard_failures_write_at_level_zero_while_gated_logs_do_not() {
    let _state = LoggingState::acquire();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("console.log");
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    *FILE_SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(file);
    store_level(0);

    vlog(format_args!("gated-line-marker"));
    vlog_err(format_args!("hard-failure-marker"));

    let written = fs::read_to_string(&path).unwrap();
    assert!(
        written.contains("hard-failure-marker"),
        "a hard failure must reach console.log even at level 0"
    );
    assert!(
        !written.contains("gated-line-marker"),
        "a gated log must stay silent at level 0"
    );
}

#[test]
fn effective_level_takes_the_max() {
    assert_eq!(effective_level(0, 0), 0);
    assert_eq!(effective_level(2, 0), 2);
    assert_eq!(effective_level(0, 3), 3);
    assert_eq!(effective_level(1, 2), 2);
    assert_eq!(effective_level(3, 1), 3);
}

#[test]
fn archive_name_formats_date_then_time() {
    let dt = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::June, 7).unwrap(),
        Time::from_hms(14, 30, 5).unwrap(),
    )
    .assume_utc();
    assert_eq!(archive_name(dt), "console-2026-06-07_14-30-05-000.log");
}

#[test]
fn prune_logs_keeps_only_the_newest() {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "console-2026-06-01_10-00-00.log",
        "console-2026-06-02_10-00-00.log",
        "console-2026-06-03_10-00-00.log",
        "console-2026-06-04_10-00-00.log",
        "console-2026-06-05_10-00-00.log",
    ] {
        fs::write(dir.path().join(name), b"x").unwrap();
    }
    // an unrelated file must be left untouched
    fs::write(dir.path().join("notes.txt"), b"keep").unwrap();

    prune_logs(dir.path(), 3);

    let mut remaining: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    remaining.sort();
    assert_eq!(
        remaining,
        vec![
            "console-2026-06-03_10-00-00.log".to_string(),
            "console-2026-06-04_10-00-00.log".to_string(),
            "console-2026-06-05_10-00-00.log".to_string(),
            "notes.txt".to_string(),
        ]
    );
}

#[test]
fn rotate_moves_leftover_into_logs_dir() {
    let dir = tempfile::tempdir().unwrap();
    let current = dir.path().join("console.log");
    std::fs::write(&current, b"previous session\n").unwrap();

    rotate_console_log(dir.path());

    // The root console.log is gone (rotation only moves; the fresh one is
    // created later by ensure_file_sink).
    assert!(!current.exists());

    let logs_dir = dir.path().join("logs");
    let archived: Vec<_> = std::fs::read_dir(&logs_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("console-") && n.ends_with(".log"))
        })
        .collect();
    assert_eq!(archived.len(), 1);
    assert_eq!(
        std::fs::read_to_string(&archived[0]).unwrap(),
        "previous session\n"
    );
}

#[test]
fn rotate_is_noop_without_leftover() {
    let dir = tempfile::tempdir().unwrap();
    rotate_console_log(dir.path()); // must not panic / must not create logs/
    assert!(!dir.path().join("logs").exists());
}

#[test]
fn store_level_roundtrips_through_log_level() {
    let _state = LoggingState::acquire();
    store_level(2);
    assert_eq!(log_level(), 2);
    store_level(0);
    assert_eq!(log_level(), 0);
    store_level(3);
    assert_eq!(log_level(), 3);
}
