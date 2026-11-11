//! What the settings tests share, attached to `src/settings/mod.rs` by `#[path]`.

use super::init_schema;
use rusqlite::Connection;
use std::path::Path;

/// A database of its own for each test, the schema in place.
pub(super) fn mem_conn() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    init_schema(&mut conn).unwrap();
    conn
}

/// A database in a file, with a log like the app's: an in-memory one has nothing to sync.
pub(super) fn file_conn(dir: &Path) -> Connection {
    let mut conn = Connection::open(dir.join("settings.db")).unwrap();
    init_schema(&mut conn).unwrap();
    conn
}
