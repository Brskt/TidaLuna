//! Tests for the saved output level in `src/settings.rs`, attached by `#[path]`.

use super::fixture::mem_conn;
use super::*;

#[test]
fn nothing_is_saved_before_a_first_level() {
    assert_eq!(load_output_level(&mut mem_conn()), None);
}

#[test]
fn save_then_load_roundtrips_both_levels() {
    let mut conn = mem_conn();
    save_output_level(&mut conn, 0.0, 62.5);
    assert_eq!(load_output_level(&mut conn), Some((0.0, 62.5)));
}

#[test]
fn a_level_without_the_one_to_restore_reads_as_nothing_saved() {
    let mut conn = mem_conn();
    set(&conn, PLAYER_VOLUME.key, "40");
    assert_eq!(load_output_level(&mut conn), None);
    set(&conn, PLAYER_VOLUME_UNMUTE.key, "forty");
    assert_eq!(load_output_level(&mut conn), None);
}
