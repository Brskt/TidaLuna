//! Tests for `src/logging.rs`, attached to it by `#[path]`.

use super::*;
use std::fs;

static LOGGING_STATE: Mutex<()> = Mutex::new(());

/// libtest runs the crate in one process, several threads wide. A test that
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
        // The ungated budget is process-wide, and every other test that trips a `verr!` spends
        // from it. A test asserting on what reached the sink has to start from a full one.
        ERR_STATE.store(u64::from(ERR_BURST), Ordering::Relaxed);
        ERR_MISSED.store(0, Ordering::Relaxed);
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

/// `verr!` is never level-gated, so anything an untrusted caller can repeat reaches the disk at
/// every level. The budget is what stands between a loop and the persistent log; its shape is the
/// kernel's `___ratelimit`, where the flood's own next call past the window is what reports the
/// drops and re-arms the burst.
#[test]
fn the_ungated_path_spends_a_burst_then_counts_what_it_drops() {
    let state = AtomicU64::new(u64::from(ERR_BURST));
    let missed = AtomicU32::new(0);

    for _ in 0..ERR_BURST {
        assert!(matches!(err_budget(0, &state, &missed), ErrBudget::Write));
    }

    for _ in 0..100 {
        assert!(matches!(err_budget(0, &state, &missed), ErrBudget::Drop));
    }
    assert_eq!(missed.load(Ordering::Relaxed), 100);

    assert!(matches!(
        err_budget(ERR_INTERVAL_MS, &state, &missed),
        ErrBudget::WroteAfterMissing(100)
    ));
    assert_eq!(
        missed.load(Ordering::Relaxed),
        0,
        "the report clears what it reported"
    );
}

/// A caller that trickles below the burst is never suppressed, however long it runs: each window
/// it crosses re-arms the whole budget. Only a burst inside one window costs anything.
#[test]
fn a_slow_caller_never_spends_its_budget() {
    let state = AtomicU64::new(u64::from(ERR_BURST));
    let missed = AtomicU32::new(0);

    for window in 0..50 {
        let now = window * ERR_INTERVAL_MS;
        assert!(matches!(
            err_budget(now, &state, &missed),
            ErrBudget::Write | ErrBudget::WroteAfterMissing(0)
        ));
    }
    assert_eq!(missed.load(Ordering::Relaxed), 0);
}

/// `verr!` is reachable from CEF, request and audio threads at once, and the budget is what an
/// untrusted caller races. Held as two atomics it leaked twice: a pair reading the last unit both
/// subtracted and wrapped the cap away, a pair finding the window expired both took a write.
/// Statistical, not deterministic: the compare-and-swap is what holds, this only guards it.
#[test]
fn a_contended_budget_never_grants_more_than_its_burst() {
    const THREADS: u32 = 8;
    const EACH: u32 = 500;
    const ROUNDS: u32 = 20;

    for _ in 0..ROUNDS {
        let state = AtomicU64::new(u64::from(ERR_BURST));
        let missed = AtomicU32::new(0);
        let granted = AtomicU32::new(0);

        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| {
                    for _ in 0..EACH {
                        // A fixed `now` keeps every call inside the one window under test, so any
                        // grant past the burst is the race and not a legitimate re-arm.
                        match err_budget(1, &state, &missed) {
                            ErrBudget::Write | ErrBudget::WroteAfterMissing(_) => {
                                granted.fetch_add(1, Ordering::Relaxed);
                            }
                            ErrBudget::Drop => {}
                        }
                    }
                });
            }
        });

        assert_eq!(
            granted.load(Ordering::Relaxed),
            ERR_BURST,
            "the window granted more writes than its burst allows"
        );
        assert_eq!(
            state.load(Ordering::Relaxed) & ERR_LEFT_MASK,
            0,
            "a wrapped budget reads as a huge remainder, not as a spent one"
        );
        assert_eq!(
            missed.load(Ordering::Relaxed),
            THREADS * EACH - ERR_BURST,
            "every refused call owes the report a line"
        );
    }
}

/// Fails every write the way a stderr that outlived its pipe or its disk does.
struct DeadConsole(std::io::ErrorKind);

impl Write for DeadConsole {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(self.0.into())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(self.0.into())
    }
}

#[test]
fn a_dead_console_costs_the_line_neither_a_panic_nor_its_file_copy() {
    for kind in [
        std::io::ErrorKind::BrokenPipe,
        std::io::ErrorKind::StorageFull,
        std::io::ErrorKind::WouldBlock,
    ] {
        let sink = Mutex::new(Some(Vec::new()));
        write_line(&mut DeadConsole(kind), &sink, "[00:00:00:000] kept\n");
        assert_eq!(
            sink.into_inner().unwrap(),
            Some(b"[00:00:00:000] kept\n".to_vec()),
            "{kind:?}"
        );
    }
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

/// The one `console-*.log` under `<dir>/logs`, or a panic naming what was there.
fn sole_archive(dir: &Path) -> std::path::PathBuf {
    let mut found: Vec<_> = fs::read_dir(dir.join("logs"))
        .unwrap_or_else(|e| panic!("no logs dir: {e}"))
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("console-") && n.ends_with(".log"))
        })
        .collect();
    found.sort();
    assert_eq!(
        found.len(),
        1,
        "expected exactly one archive, got {found:?}"
    );
    found.remove(0)
}

#[test]
fn an_empty_slot_lets_the_leftover_be_archived() {
    // Pins that the empty-slot path really reaches the rotation, a gutted
    // `adopt_leftover` still satisfying the filled-slot test below.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("console.log"), b"previous session\n").unwrap();

    let mut sink = None;
    adopt_leftover(dir.path(), &mut sink);

    assert!(!dir.path().join("console.log").exists());
    assert_eq!(
        fs::read_to_string(sole_archive(dir.path())).unwrap(),
        "previous session\n"
    );
}

#[test]
fn a_slot_that_already_holds_a_handle_rotates_nothing() {
    // Renaming a `console.log` this process is holding succeeds silently on both
    // platforms, and the session would then write into the archive while the
    // path no longer existed. An empty slot is what rules that out.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("console.log");
    fs::write(&path, b"live session\n").unwrap();
    let open = fs::OpenOptions::new().append(true).open(&path).unwrap();

    let mut sink = Some(open);
    adopt_leftover(dir.path(), &mut sink);

    assert!(
        !dir.path().join("logs").exists(),
        "a filled slot must not rotate the file it is already writing to"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "live session\n");
}
