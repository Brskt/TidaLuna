//! Tests for the crossfade settings in `src/settings.rs`, attached by `#[path]`.

use super::*;
use rusqlite::Connection;

fn mem_conn() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    init_schema(&mut conn).unwrap();
    conn
}

#[test]
fn crossfade_is_off_by_default() {
    let mut conn = mem_conn();
    assert!(!load_crossfade_enabled(&mut conn));
    assert_eq!(load_crossfade_secs(&mut conn), 0);
}

#[test]
fn save_then_load_enabled_roundtrips() {
    let mut conn = mem_conn();
    save_crossfade_enabled(&mut conn, true);
    assert!(load_crossfade_enabled(&mut conn));
    save_crossfade_enabled(&mut conn, false);
    assert!(!load_crossfade_enabled(&mut conn));
}

#[test]
fn save_then_load_secs_roundtrips() {
    let mut conn = mem_conn();
    save_crossfade_secs(&mut conn, 6);
    assert_eq!(load_crossfade_secs(&mut conn), 6);
}

#[test]
fn secs_clamp_to_the_maximum_on_write_and_on_read() {
    let mut conn = mem_conn();
    // A caller that ignores the range cannot store an out-of-range value.
    save_crossfade_secs(&mut conn, 99);
    assert_eq!(load_crossfade_secs(&mut conn), 12);
    // Nor can a value hand-edited into the database escape it.
    set(&conn, "player.crossfade_secs", "250");
    assert_eq!(load_crossfade_secs(&mut conn), 12);
}

#[test]
fn an_unparseable_stored_value_falls_back_to_off() {
    let mut conn = mem_conn();
    set(&conn, "player.crossfade_secs", "six");
    assert_eq!(load_crossfade_secs(&mut conn), 0);
    set(&conn, "player.crossfade_enabled", "yes-please");
    assert!(!load_crossfade_enabled(&mut conn));
}

#[test]
fn boot_settings_carry_the_crossfade_values() {
    let mut conn = mem_conn();
    save_crossfade_enabled(&mut conn, true);
    save_crossfade_secs(&mut conn, 6);
    let boot = load_boot_settings(&mut conn);
    assert!(boot.crossfade_enabled);
    assert_eq!(boot.crossfade_secs, 6);
}
