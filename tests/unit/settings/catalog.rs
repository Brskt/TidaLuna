//! Tests for `src/settings/catalog.rs`, attached to it by `#[path]`.

use super::*;
use crate::settings::fixture::{
    self, assert_in_order, file_conn, mem_conn, root_page, ruin_page, seeded, statements_of,
    syncs_fully, syncs_routinely,
};

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

#[test]
fn boot_settings_carry_the_video_codec_switch() {
    let mut conn = mem_conn();
    // `false`, the value the default cannot produce: a loader that ignored the row would pass `true`.
    save_video_codec_enabled(&mut conn, false);
    let boot = load_boot_settings(&mut conn);
    assert!(!boot.video_codec_enabled);

    // And back: a loader that answered `false` whatever the row said would pass the first half.
    save_video_codec_enabled(&mut conn, true);
    assert!(load_boot_settings(&mut conn).video_codec_enabled);
}

#[test]
fn a_level_without_the_one_to_restore_reads_as_nothing_saved() {
    let mut conn = mem_conn();
    set(&conn, PLAYER_VOLUME.key, "40");
    assert_eq!(load_output_level(&mut conn), None);
    set(&conn, PLAYER_VOLUME_UNMUTE.key, "forty");
    assert_eq!(load_output_level(&mut conn), None);
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

#[test]
fn a_refused_consent_is_written_under_a_full_sync() {
    let savers: [fn(&mut Connection, bool); 3] = [
        save_video_codec_enabled,
        save_receiver_always_on,
        save_plugin_update_at_boot,
    ];
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

#[test]
fn damage_after_the_open_reads_as_refused_and_is_reported_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    let conn = file_conn(dir.path());
    ruin_page(&path, root_page(&path, "settings"));
    crate::logging::take_lines();

    {
        let mut reader = Reader::new(&conn);
        assert!(!reader.get(&VIDEO_CODEC_ENABLED));
        assert!(!reader.get(&CONNECT_RECEIVER_ALWAYS_ON));
        // Never written: the intact index answers "no such key" without reading the table page.
        assert!(reader.get(&UPDATER_AUTO_CHECK));
        let failures = reader.into_failures();
        assert_eq!(failures.len(), 2);
        assert!(
            failures
                .iter()
                .all(|(_, why)| matches!(why, Unreadable::Db(_)))
        );
    }
    assert!(crate::logging::take_lines().is_empty());

    {
        let mut reader = Reader::new(&conn);
        reader.get(&VIDEO_CODEC_ENABLED);
        reader.get(&CONNECT_RECEIVER_ALWAYS_ON);
    }
    let lines = crate::logging::take_lines();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("2 unreadable"));
}

#[test]
fn a_damaged_value_is_unparseable_and_reads_as_refused() {
    let conn = fixture::mem_conn();
    set(&conn, VIDEO_CODEC_ENABLED.key, "talse");
    let mut reader = Reader::new(&conn);
    assert!(!reader.get(&VIDEO_CODEC_ENABLED));
    let failures = reader.into_failures();
    assert!(matches!(
        failures.as_slice(),
        [(_, Unreadable::Unparseable(raw))] if raw == "talse"
    ));
}
