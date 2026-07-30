//! Tests for `src/settings.rs`, attached to it by `#[path]`.

use super::*;
use rusqlite::Connection;

fn mem_conn() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    init_schema(&mut conn).unwrap();
    conn
}

#[test]
fn get_u8_returns_default_when_absent() {
    let conn = mem_conn();
    assert_eq!(get_u8(&conn, "logging.level", 0), 0);
    assert_eq!(get_u8(&conn, "logging.level", 2), 2);
}

#[test]
fn get_u8_parses_stored_value() {
    let conn = mem_conn();
    set(&conn, "logging.level", "2");
    assert_eq!(get_u8(&conn, "logging.level", 0), 2);
}

#[test]
fn get_u8_falls_back_on_unparseable() {
    let conn = mem_conn();
    set(&conn, "logging.level", "not-a-number");
    assert_eq!(get_u8(&conn, "logging.level", 1), 1);
}

#[test]
fn load_log_level_clamps_to_three() {
    let mut conn = mem_conn();
    set(&conn, "logging.level", "99");
    assert_eq!(load_log_level(&mut conn), 3);
}

#[test]
fn save_then_load_log_level_roundtrips() {
    let mut conn = mem_conn();
    save_log_level(&mut conn, 2);
    assert_eq!(load_log_level(&mut conn), 2);
}

#[test]
fn save_then_load_console_roundtrips() {
    let mut conn = mem_conn();
    assert!(!load_console(&mut conn)); // default false
    save_console(&mut conn, true);
    assert!(load_console(&mut conn));
}
