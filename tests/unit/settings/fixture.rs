//! What the settings tests share, and what the trust and plugin store tests reuse of it, attached
//! to `src/settings/mod.rs` by `#[path]`.

use super::catalog::{CONNECT_RECEIVER_ALWAYS_ON, PLUGINS_UPDATE_AT_BOOT, VIDEO_CODEC_ENABLED};
use super::{
    init_schema, save_log_level, save_plugin_update_at_boot, save_receiver_always_on,
    save_video_codec_enabled,
};
use rusqlite::Connection;
use std::cell::RefCell;
use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// A database of its own for each test, the schema in place.
pub(crate) fn mem_conn() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    init_schema(&mut conn).unwrap();
    conn
}

/// A database in a file, with a log like the app's: an in-memory one has nothing to sync.
pub(crate) fn file_conn(dir: &Path) -> Connection {
    let mut conn = Connection::open(dir.join("settings.db")).unwrap();
    init_schema(&mut conn).unwrap();
    conn
}

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

pub(super) const PAGE: u64 = 4096;

pub(super) fn root_page(path: &Path, name: &str) -> u64 {
    let conn = Connection::open(path).unwrap();
    let page: i64 = conn
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name = ?1",
            [name],
            |row| row.get(0),
        )
        .unwrap();
    page as u64
}

pub(super) fn overwrite(path: &Path, offset: u64, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(bytes).unwrap();
}

pub(super) fn ruin_page(path: &Path, page: u64) {
    overwrite(path, (page - 1) * PAGE, &[0xAB; PAGE as usize]);
}

pub(super) fn insert_refused_trust(conn: &Connection) {
    conn.execute(
        "INSERT INTO native_trust (code_hash, plugin, module, granted) VALUES ('h', 'p', 'p/m.native.ts', 0)",
        [],
    )
    .unwrap();
}

/// A profile with a log level and a trust row, and the three default-on consents refused or left
/// without a row.
pub(super) fn profile(dir: &Path, refused_consents: bool) -> PathBuf {
    let path = dir.join("settings.db");
    let mut conn = Connection::open(&path).unwrap();
    init_schema(&mut conn).unwrap();
    if refused_consents {
        save_video_codec_enabled(&mut conn, false);
        save_receiver_always_on(&mut conn, false);
        save_plugin_update_at_boot(&mut conn, false);
    }
    save_log_level(&mut conn, 2);
    insert_refused_trust(&conn);
    path
}

/// A profile that refused the three default-on consents and kept a log level, as the real one does.
pub(super) fn seeded(dir: &Path) -> PathBuf {
    profile(dir, true)
}

pub(super) fn page_image(path: &Path, page: u64) -> Vec<u8> {
    let file = std::fs::read(path).unwrap();
    file[((page - 1) * PAGE) as usize..(page * PAGE) as usize].to_vec()
}

pub(super) const CONSENTS: [&str; 3] = [
    PLUGINS_UPDATE_AT_BOOT.key,
    CONNECT_RECEIVER_ALWAYS_ON.key,
    VIDEO_CODEC_ENABLED.key,
];

/// The copies of damaged stores kept beside `path`, companions left out, oldest first.
pub(super) fn asides(path: &Path) -> Vec<PathBuf> {
    let prefix = format!("{}.corrupt-", path.file_name().unwrap().to_string_lossy());
    let mut found: Vec<PathBuf> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with(&prefix) && !name.ends_with("-wal") && !name.ends_with("-shm")
        })
        .map(|entry| entry.path())
        .collect();
    found.sort();
    found
}

/// A directory with something in it where a file must go: nothing can be written there.
pub(super) fn block(path: &Path) {
    std::fs::create_dir(path).unwrap();
    std::fs::write(path.join("occupied"), b"x").unwrap();
}
