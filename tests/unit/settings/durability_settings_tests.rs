//! Tests for what a settings write waits for, in `src/settings.rs`, attached by `#[path]`.

use super::fixture::file_conn;
use super::*;
use std::cell::RefCell;
use std::ffi::{CStr, c_char, c_int, c_uint, c_void};

thread_local! {
    static STATEMENTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

extern "C" fn record_statement(
    _kind: c_uint,
    _ctx: *mut c_void,
    _stmt: *mut c_void,
    sql: *mut c_void,
) -> c_int {
    // SAFETY: for `SQLITE_TRACE_STMT` the fourth argument is the statement's NUL-terminated
    // text, valid for the duration of the callback.
    let text = unsafe { CStr::from_ptr(sql as *const c_char) };
    STATEMENTS.with(|list| {
        list.borrow_mut()
            .push(text.to_string_lossy().to_lowercase())
    });
    0
}

/// Every statement `conn` runs from now on, as `write` runs them, in order.
pub(crate) fn statements_of(
    conn: &mut Connection,
    write: impl FnOnce(&mut Connection),
) -> Vec<String> {
    STATEMENTS.with(|list| list.borrow_mut().clear());
    // SAFETY: the handle is the open connection's, and the callback takes no context.
    unsafe {
        rusqlite::ffi::sqlite3_trace_v2(
            conn.handle(),
            rusqlite::ffi::SQLITE_TRACE_STMT,
            Some(record_statement),
            std::ptr::null_mut(),
        );
    }
    write(conn);
    STATEMENTS.with(|list| std::mem::take(&mut *list.borrow_mut()))
}

pub(crate) fn position(statements: &[String], wanted: impl Fn(&str) -> bool) -> Option<usize> {
    statements.iter().position(|sql| wanted(sql))
}

pub(crate) fn syncs_fully(sql: &str) -> bool {
    sql.contains("synchronous") && sql.contains("full")
}

pub(crate) fn syncs_routinely(sql: &str) -> bool {
    sql.contains("synchronous") && sql.contains("normal")
}

/// The positions of `wanted` in `statements`, each found and each after the one before.
pub(crate) fn assert_in_order(statements: &[String], wanted: &[&dyn Fn(&str) -> bool]) {
    let mut last = None;
    for (n, matches) in wanted.iter().enumerate() {
        let at = position(statements, matches)
            .unwrap_or_else(|| panic!("statement {n} never ran: {statements:?}"));
        assert!(
            last.is_none_or(|before| before < at),
            "statement {n} ran out of order: {statements:?}"
        );
        last = Some(at);
    }
}

/// The commit of the write is the statement between the two pragmas: a sync asked for after it
/// would be asked too late.
fn assert_written_under_full_sync(statements: &[String]) {
    assert_in_order(
        statements,
        &[
            &syncs_fully,
            &|sql| sql.starts_with("insert or replace"),
            &syncs_routinely,
        ],
    );
}

/// What a power cut leaves of the store `file` in `dir` when only the first `wal_kept` bytes of its
/// log reached the disk: the commits are a prefix, the log's checksums chain from its first frame.
pub(crate) fn after_a_power_cut(dir: &Path, file: &str, wal_kept: u64) -> Connection {
    let cut = dir.join(format!("cut-{wal_kept}"));
    std::fs::create_dir_all(&cut).unwrap();
    std::fs::copy(dir.join(file), cut.join(file)).unwrap();
    let wal = std::fs::read(dir.join(format!("{file}-wal"))).unwrap();
    std::fs::write(cut.join(format!("{file}-wal")), &wal[..wal_kept as usize]).unwrap();
    Connection::open(cut.join(file)).unwrap()
}

#[test]
fn a_refused_consent_is_written_under_a_full_sync() {
    let savers: [fn(&mut Connection, bool); 2] =
        [save_video_codec_enabled, save_receiver_always_on];
    for save in savers {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = file_conn(dir.path());
        let statements = statements_of(&mut conn, |c| save(c, false));
        assert_written_under_full_sync(&statements);
    }
}

#[test]
fn a_refusal_inside_a_transaction_is_written_and_reported_unsynced() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = file_conn(dir.path());
    crate::logging::take_lines();
    {
        let tx = conn.unchecked_transaction().unwrap();
        VIDEO_CODEC_ENABLED.save(&tx, "false");
        tx.commit().unwrap();
    }
    let lines = crate::logging::take_lines();
    assert!(
        lines.iter().any(|line| line.contains("cannot sync")),
        "{lines:?}"
    );
    assert!(!load_video_codec_enabled(&mut conn));
    let level: i64 = conn
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .unwrap();
    assert_eq!(level, 1);
}
