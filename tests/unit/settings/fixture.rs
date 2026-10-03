//! What the settings tests share, attached to `src/settings.rs` by `#[path]`.

use super::{Connection, init_schema};

/// A database of its own for each test, the schema in place.
pub(super) fn mem_conn() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    init_schema(&mut conn).unwrap();
    conn
}
