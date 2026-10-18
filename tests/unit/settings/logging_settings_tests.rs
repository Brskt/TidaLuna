//! Tests for `src/settings.rs`, attached to it by `#[path]`.

use super::fixture::mem_conn;
use super::*;

#[test]
fn the_log_level_is_off_when_absent() {
    let mut conn = mem_conn();
    assert_eq!(load_log_level(&mut conn), 0);
}

#[test]
fn load_log_level_clamps_to_three() {
    let mut conn = mem_conn();
    set(&conn, LOGGING_LEVEL.key, "99");
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
