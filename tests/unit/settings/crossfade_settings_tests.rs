//! Tests for the crossfade settings in `src/settings.rs`, attached by `#[path]`.

use super::fixture::mem_conn;
use super::*;

#[test]
fn secs_clamp_to_the_maximum_on_write_and_on_read() {
    let mut conn = mem_conn();
    // A caller that ignores the range cannot store an out-of-range value.
    save_crossfade_secs(&mut conn, 99);
    assert_eq!(Reader::new(&conn).get(&PLAYER_CROSSFADE_SECS), 12);
    assert_eq!(load_crossfade_secs(&mut conn), 12);
    // Nor can a value hand-edited into the database escape it.
    set(&conn, PLAYER_CROSSFADE_SECS.key, "250");
    assert_eq!(load_crossfade_secs(&mut conn), 12);
}
