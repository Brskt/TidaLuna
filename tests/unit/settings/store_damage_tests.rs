//! Tests for a settings store that is damaged, attached to `src/settings.rs` by `#[path]`.

use super::*;
use std::io::{Seek, SeekFrom, Write};

const PAGE: u64 = 4096;

fn root_page(path: &Path, name: &str) -> u64 {
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

fn overwrite(path: &Path, offset: u64, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(bytes).unwrap();
}

fn ruin_page(path: &Path, page: u64) {
    overwrite(path, (page - 1) * PAGE, &[0xAB; PAGE as usize]);
}

fn insert_refused_trust(conn: &Connection) {
    conn.execute(
        "INSERT INTO native_trust (code_hash, plugin, module, granted) VALUES ('h', 'p', 'p/m.native.ts', 0)",
        [],
    )
    .unwrap();
}

/// A profile with a log level and a trust row, and the two default-on consents refused or left
/// without a row.
fn profile(dir: &Path, refused_consents: bool) -> PathBuf {
    let path = dir.join("settings.db");
    let mut conn = Connection::open(&path).unwrap();
    init_schema(&mut conn).unwrap();
    if refused_consents {
        save_video_codec_enabled(&mut conn, false);
        save_receiver_always_on(&mut conn, false);
    }
    save_log_level(&mut conn, 2);
    insert_refused_trust(&conn);
    path
}

/// A profile that refused the two default-on consents and kept a log level, as the real one does.
fn seeded(dir: &Path) -> PathBuf {
    profile(dir, true)
}

#[test]
fn damage_after_the_open_reads_as_refused_and_is_reported_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    let mut conn = Connection::open(&path).unwrap();
    init_schema(&mut conn).unwrap();
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

#[test]
fn a_boot_read_on_a_failing_store_reports_one_line_and_reads_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    let mut conn = Connection::open(&path).unwrap();
    init_schema(&mut conn).unwrap();
    ruin_page(&path, root_page(&path, "settings"));
    crate::logging::take_lines();

    let boot = load_boot_settings(&mut conn);

    assert!(!boot.video_codec_enabled);
    assert!(!boot.receiver_always_on);
    assert!(boot.auto_check);
    let lines = crate::logging::take_lines();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("unreadable"));
}
