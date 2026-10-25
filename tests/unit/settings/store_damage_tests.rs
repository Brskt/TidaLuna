//! Tests for a settings store that is damaged, attached to `src/settings.rs` by `#[path]`.

use super::*;
use std::io::{Seek, SeekFrom, Write};

const PAGE: u64 = 4096;

/// What a table row does to the file before it is opened.
type Damage = fn(&Path);

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

/// A profile that never touched the two default-on consents: no row for either.
fn untouched(dir: &Path) -> PathBuf {
    profile(dir, false)
}

/// A store whose table page went back to before the video refusal while the index kept it: a
/// write that never reached the disk. `after_refusal` runs on the connection that wrote it.
fn table_page_reverted_index_kept(dir: &Path, after_refusal: impl FnOnce(&Connection)) -> PathBuf {
    let path = dir.join("settings.db");
    {
        let mut conn = Connection::open(&path).unwrap();
        init_schema(&mut conn).unwrap();
        save_receiver_always_on(&mut conn, false);
        save_log_level(&mut conn, 2);
    }
    let table = root_page(&path, "settings");
    let before = page_image(&path, table);
    {
        let mut conn = Connection::open(&path).unwrap();
        save_video_codec_enabled(&mut conn, false);
        after_refusal(&conn);
    }
    overwrite(&path, (table - 1) * PAGE, &before);
    path
}

/// Overwrites a byte of the video refusal's key in the settings index: a read through it finds
/// nothing while the table still holds the row.
fn corrupt_index_byte(path: &Path) {
    let index = root_page(path, "sqlite_autoindex_settings_1");
    let page = std::fs::read(path).unwrap();
    let from = ((index - 1) * PAGE) as usize;
    let at = page[from..from + PAGE as usize]
        .windows(VIDEO_CODEC_ENABLED.key.len())
        .position(|w| w == VIDEO_CODEC_ENABLED.key.as_bytes())
        .unwrap();
    overwrite(path, (from + at + 18) as u64, b"e");
}

fn page_image(path: &Path, page: u64) -> Vec<u8> {
    let file = std::fs::read(path).unwrap();
    file[((page - 1) * PAGE) as usize..(page * PAGE) as usize].to_vec()
}

const BOTH_CONSENTS: [&str; 2] = [CONNECT_RECEIVER_ALWAYS_ON.key, VIDEO_CODEC_ENABLED.key];

fn trust_rows(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM native_trust", [], |row| row.get(0))
        .unwrap()
}

/// The copies of damaged stores kept beside `path`, companions left out, oldest first.
fn asides(path: &Path) -> Vec<PathBuf> {
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

/// Every file beside `path` that is a leftover of a rebuild: its working name or a companion.
fn leftovers(path: &Path) -> Vec<PathBuf> {
    ["-journal", ".new", ".new-wal", ".new-shm", ".new-journal"]
        .iter()
        .map(|suffix| beside(path, suffix))
        .filter(|p| p.exists())
        .collect()
}

/// A directory with something in it where a file must go: nothing can be written there.
fn block(path: &Path) {
    std::fs::create_dir(path).unwrap();
    std::fs::write(path.join("occupied"), b"x").unwrap();
}

#[test]
fn a_damaged_settings_page_is_set_aside_and_the_refusals_survive_the_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    ruin_page(&path, root_page(&path, "settings"));

    let (mut conn, opened) = open(&path).unwrap();

    assert_eq!(
        opened,
        Opened::Rebuilt {
            turned_off: BOTH_CONSENTS.to_vec(),
            trust: Trust::Dropped
        }
    );
    assert_eq!(asides(&path).len(), 1);
    assert!(!load_video_codec_enabled(&mut conn));
    assert!(!load_receiver_always_on(&mut conn));
    assert!(load_update_auto_check(&mut conn));
}

#[test]
fn a_store_that_is_not_a_database_is_rebuilt_instead_of_failing_the_launch() {
    let damages: [(&str, Damage); 2] = [
        ("a header that is not sqlite", |path| {
            overwrite(path, 0, b"this is not a database header")
        }),
        ("a file cut to its first page", |path| {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            file.set_len(PAGE).unwrap();
        }),
    ];
    for (what, damage) in damages {
        let dir = tempfile::tempdir().unwrap();
        let path = seeded(dir.path());
        damage(&path);

        assert_eq!(
            open(&path).unwrap().1,
            Opened::Rebuilt {
                turned_off: BOTH_CONSENTS.to_vec(),
                trust: Trust::Unknown
            },
            "{what}"
        );
    }
}

#[test]
fn an_index_that_lost_a_row_is_rebuilt_without_losing_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    corrupt_index_byte(&path);

    // The read the loader makes finds nothing, the same answer as a setting never written.
    {
        let conn = Connection::open(&path).unwrap();
        assert!(matches!(
            read::<bool>(&conn, VIDEO_CODEC_ENABLED.key),
            Read::Absent
        ));
    }

    let (mut conn, opened) = open(&path).unwrap();
    // The table still held both refusals: nothing to write down.
    assert_eq!(
        opened,
        Opened::Reindexed {
            turned_off: vec![],
            trust: Trust::Empty
        }
    );
    assert!(!load_video_codec_enabled(&mut conn));
}

#[test]
fn a_damaged_index_leaves_the_settings_rows_to_carry_over() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    ruin_page(&path, root_page(&path, "sqlite_autoindex_settings_1"));

    let (mut conn, opened) = open(&path).unwrap();

    assert_eq!(
        opened,
        Opened::Rebuilt {
            turned_off: vec![],
            trust: Trust::Dropped
        }
    );
    assert!(!load_video_codec_enabled(&mut conn));
    assert_eq!(load_log_level(&mut conn), 2);
}

/// Unknown, not empty, whether the table held a decision or none: even an empty table has a root
/// page to read, and a table gone from the schema is not told as empty either.
#[test]
fn a_damaged_trust_table_does_not_carry_trust_decisions_over() {
    type Profile = fn(&Path) -> PathBuf;
    let cases: [(&str, Damage, Profile, u8); 3] = [
        (
            "a ruined page",
            |path| ruin_page(path, root_page(path, "native_trust")),
            seeded,
            2,
        ),
        (
            "a ruined page of an empty table",
            |path| ruin_page(path, root_page(path, "native_trust")),
            seeded_without_trust,
            0,
        ),
        (
            "a table gone from the schema",
            |path| delete_schema_rows(path, "native_trust"),
            seeded,
            2,
        ),
    ];
    for (what, damage, profile, log_level) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = profile(dir.path());
        damage(&path);

        let (mut conn, opened) = open(&path).unwrap();

        assert_eq!(
            opened,
            Opened::Rebuilt {
                turned_off: vec![],
                trust: Trust::Unknown
            },
            "{what}"
        );
        assert_eq!(trust_rows(&conn), 0, "{what}");
        assert_eq!(load_log_level(&mut conn), log_level, "{what}");
    }
}

#[test]
fn a_store_that_cannot_be_opened_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.db");
    std::fs::create_dir(&path).unwrap();

    assert!(open(&path).is_err());
    assert!(asides(&path).is_empty());
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
fn a_row_the_user_writes_after_a_loss_replaces_its_safe_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    ruin_page(&path, root_page(&path, "settings"));
    let (mut conn, _) = open(&path).unwrap();
    assert!(!load_video_codec_enabled(&mut conn));
    save_video_codec_enabled(&mut conn, true);
    assert!(load_video_codec_enabled(&mut conn));
}

#[test]
fn damage_confined_to_the_trust_table_leaves_untouched_consents_on() {
    let dir = tempfile::tempdir().unwrap();
    let path = untouched(dir.path());
    ruin_page(&path, root_page(&path, "native_trust"));

    let (mut conn, opened) = open(&path).unwrap();

    assert_eq!(
        opened,
        Opened::Rebuilt {
            turned_off: vec![],
            trust: Trust::Unknown
        }
    );
    assert!(load_video_codec_enabled(&mut conn));
    assert!(load_receiver_always_on(&mut conn));
    assert_eq!(load_log_level(&mut conn), 2);
}

#[test]
fn a_refusal_the_table_lost_is_not_laundered_by_reindex() {
    let dir = tempfile::tempdir().unwrap();
    let path = table_page_reverted_index_kept(dir.path(), |_| {});

    let (mut conn, opened) = open(&path).unwrap();

    assert_eq!(
        opened,
        Opened::Reindexed {
            turned_off: vec![VIDEO_CODEC_ENABLED.key],
            trust: Trust::Empty
        }
    );
    assert!(!load_video_codec_enabled(&mut conn));
    assert!(!load_receiver_always_on(&mut conn));
}

#[test]
fn a_repair_is_told_only_when_the_user_would_notice_it() {
    let codec = || vec![VIDEO_CODEC_ENABLED.key];
    assert_eq!(Opened::Healthy.notice(), None);
    assert_eq!(
        Opened::Reindexed {
            turned_off: vec![],
            trust: Trust::Empty
        }
        .notice(),
        None
    );
    assert_eq!(
        Opened::Reindexed {
            turned_off: codec(),
            trust: Trust::Empty
        }
        .notice(),
        Some(RepairNotice {
            turned_off: codec(),
            trust: Trust::Empty,
            saved: true
        })
    );
    // A rebuild hands on what it read of the trust table, whatever it is.
    for trust in [Trust::Empty, Trust::Dropped, Trust::Unknown] {
        assert_eq!(
            Opened::Rebuilt {
                turned_off: vec![],
                trust
            }
            .notice(),
            Some(RepairNotice {
                turned_off: vec![],
                trust,
                saved: true
            })
        );
        assert_eq!(
            Opened::Volatile {
                turned_off: codec(),
                trust
            }
            .notice(),
            Some(RepairNotice {
                turned_off: codec(),
                trust,
                saved: false
            })
        );
    }
}

fn told_of(turned_off: &[&'static str], trust: Trust, saved: bool) -> Option<RepairNotice> {
    Some(RepairNotice {
        turned_off: turned_off.to_vec(),
        trust,
        saved,
    })
}

#[test]
fn a_repair_notice_outlives_the_process_that_made_it_until_it_is_acknowledged() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    ruin_page(&path, root_page(&path, "settings"));
    let told = told_of(&BOTH_CONSENTS, Trust::Dropped, true);
    {
        let (conn, _) = open(&path).unwrap();
        assert_eq!(pending_notice(&conn), told);
    }
    // The process ended before the page asked: the next launch still owes the banner, however
    // many launches pass.
    for _ in 0..2 {
        let (conn, opened) = open(&path).unwrap();
        assert_eq!(opened, Opened::Healthy);
        assert_eq!(pending_notice(&conn), told);
    }
    let (conn, _) = open(&path).unwrap();
    acknowledge_repair_notice(&conn);
    drop(conn);
    let (conn, opened) = open(&path).unwrap();
    assert_eq!(opened, Opened::Healthy);
    assert_eq!(pending_notice(&conn), None);
}

/// What `settings.store_repair` answers: the notice in the shape the page's `StoreRepair` reads,
/// then `null` once it is acknowledged.
#[test]
fn the_reply_to_the_ask_is_the_notice_as_the_page_reads_it_then_null() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    ruin_page(&path, root_page(&path, "settings"));
    let (conn, _) = open(&path).unwrap();
    assert_eq!(
        serde_json::to_string(&pending_notice(&conn)).unwrap(),
        r#"{"turnedOff":["connect.receiver_always_on","video.codec_enabled"],"trust":"dropped","saved":true}"#
    );
    acknowledge_repair_notice(&conn);
    assert_eq!(
        serde_json::to_string(&pending_notice(&conn)).unwrap(),
        "null"
    );
}

#[test]
fn a_reindex_that_turned_a_setting_off_leaves_its_notice_in_the_same_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = table_page_reverted_index_kept(dir.path(), |_| {});
    drop(open(&path).unwrap());

    let (conn, opened) = open(&path).unwrap();

    assert_eq!(opened, Opened::Healthy);
    assert_eq!(
        pending_notice(&conn),
        told_of(&[VIDEO_CODEC_ENABLED.key], Trust::Empty, true)
    );
}

#[test]
fn a_volatile_run_tells_it_cannot_save_and_leaves_nothing_on_disk_to_tell_twice() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    ruin_page(&path, root_page(&path, "settings"));
    block(&beside(&path, ".new"));

    let (conn, _) = open(&path).unwrap();
    assert_eq!(
        pending_notice(&conn),
        told_of(&BOTH_CONSENTS, Trust::Dropped, false)
    );
    acknowledge_repair_notice(&conn);
    assert_eq!(pending_notice(&conn), None);
    drop(conn);

    // The damaged file is still there; the next launch repairs it and tells of that repair
    // once.
    std::fs::remove_dir_all(beside(&path, ".new")).unwrap();
    let (conn, opened) = open(&path).unwrap();
    assert!(matches!(opened, Opened::Rebuilt { .. }));
    assert_eq!(
        pending_notice(&conn),
        told_of(&BOTH_CONSENTS, Trust::Dropped, true)
    );
}

#[test]
fn a_second_repair_before_the_first_was_told_tells_both() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    ruin_page(&path, root_page(&path, "settings"));
    drop(open(&path).unwrap());
    ruin_page(&path, root_page(&path, "native_trust"));

    let (conn, _) = open(&path).unwrap();

    assert_eq!(
        pending_notice(&conn),
        told_of(&BOTH_CONSENTS, Trust::Dropped, true)
    );
}

#[test]
fn the_notice_is_no_setting_and_no_rebuild_carries_it_as_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    ruin_page(&path, root_page(&path, "native_trust"));
    let (conn, _) = open(&path).unwrap();
    assert!(pending_notice(&conn).is_some());
    let keys: i64 = conn
        .query_row(
            "SELECT count(*) FROM settings WHERE key LIKE '%notice%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(keys, 0);
}

/// A store with nothing to repair owes no notice: a profile of this build, and stores written
/// before the notice table, or before the trust table too, which must not have one invented.
#[test]
fn a_store_from_before_the_notice_opens_healthy() {
    const BEFORE_THE_NOTICE: &str =
        "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE native_trust (
            code_hash TEXT NOT NULL, plugin TEXT NOT NULL, module TEXT NOT NULL,
            granted INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (code_hash, plugin, module));
         INSERT INTO settings VALUES ('logging.level', '2');";
    const BEFORE_THE_TRUST_TABLE: &str =
        "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO settings VALUES ('logging.level', '2');";
    for schema in [None, Some(BEFORE_THE_NOTICE), Some(BEFORE_THE_TRUST_TABLE)] {
        let dir = tempfile::tempdir().unwrap();
        let path = match schema {
            None => seeded(dir.path()),
            Some(sql) => {
                let path = dir.path().join("settings.db");
                Connection::open(&path).unwrap().execute_batch(sql).unwrap();
                path
            }
        };
        let (mut conn, opened) = open(&path).unwrap();
        assert_eq!(opened, Opened::Healthy, "{schema:?}");
        assert_eq!(pending_notice(&conn), None, "{schema:?}");
        assert_eq!(load_log_level(&mut conn), 2, "{schema:?}");
    }
}

#[test]
fn a_volatile_run_leaves_the_damaged_store_exactly_where_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    ruin_page(&path, root_page(&path, "settings"));
    block(&beside(&path, ".new"));
    let before = std::fs::read(&path).unwrap();

    let (mut conn, opened) = open(&path).unwrap();

    assert_eq!(
        opened,
        Opened::Volatile {
            turned_off: BOTH_CONSENTS.to_vec(),
            trust: Trust::Dropped
        }
    );
    assert!(!load_video_codec_enabled(&mut conn));
    assert!(!load_receiver_always_on(&mut conn));
    drop(conn);
    // Nothing moved, nothing copied: the next launch decides from the same file.
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(asides(&path).is_empty());
    std::fs::remove_dir_all(beside(&path, ".new")).unwrap();
    let (mut conn, opened) = open(&path).unwrap();
    assert_eq!(
        opened,
        Opened::Rebuilt {
            turned_off: BOTH_CONSENTS.to_vec(),
            trust: Trust::Dropped
        }
    );
    assert!(!load_video_codec_enabled(&mut conn));
}

#[test]
fn a_store_still_held_by_another_connection_is_not_replaced_under_its_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    // A session that is still there: its writes sit in the -wal, which the closing connection of
    // `open` cannot fold into the file.
    let mut holder = Connection::open(&path).unwrap();
    init_schema(&mut holder).unwrap();
    save_log_level(&mut holder, 3);
    ruin_page(&path, root_page(&path, "native_trust"));
    assert!(std::fs::metadata(beside(&path, "-wal")).unwrap().len() > 0);
    let before = std::fs::read(&path).unwrap();

    let (mut conn, opened) = open(&path).unwrap();

    assert_eq!(
        opened,
        Opened::Volatile {
            turned_off: vec![],
            trust: Trust::Unknown
        }
    );
    assert_eq!(load_log_level(&mut conn), 3);
    assert!(!load_video_codec_enabled(&mut conn));
    drop(conn);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(asides(&path).is_empty());
    assert!(leftovers(&path).is_empty());
    drop(holder);
    let (_, opened) = open(&path).unwrap();
    assert_eq!(
        opened,
        Opened::Rebuilt {
            turned_off: vec![],
            trust: Trust::Unknown
        }
    );
}

#[test]
fn a_rebuild_that_died_before_the_swap_is_redone_from_the_damaged_store() {
    // The disk a crash leaves: the damaged store, and a working file the dead run never finished
    // (garbage, a complete store with nothing of the refusals, or one written after the copy
    // aside and never renamed over the damaged store).
    let empty_store = {
        let other = tempfile::tempdir().unwrap();
        let mut fresh = Connection::open(other.path().join("x.db")).unwrap();
        init_schema(&mut fresh).unwrap();
        drop(fresh);
        std::fs::read(other.path().join("x.db")).unwrap()
    };
    for (stale, copied_aside) in [
        (b"half a write".to_vec(), false),
        (empty_store, false),
        (b"complete, but not renamed".to_vec(), true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = seeded(dir.path());
        ruin_page(&path, root_page(&path, "settings"));
        if copied_aside {
            std::fs::copy(&path, beside(&path, ".corrupt-1-0")).unwrap();
        }
        std::fs::write(beside(&path, ".new"), &stale).unwrap();
        std::fs::write(beside(&path, ".new-wal"), b"stale wal").unwrap();

        let (mut conn, opened) = open(&path).unwrap();

        assert_eq!(
            opened,
            Opened::Rebuilt {
                turned_off: BOTH_CONSENTS.to_vec(),
                trust: Trust::Dropped
            }
        );
        assert!(!load_video_codec_enabled(&mut conn));
        assert!(!load_receiver_always_on(&mut conn));
        assert!(leftovers(&path).is_empty());
    }
}

#[test]
fn the_new_store_is_never_at_the_path_without_its_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    ruin_page(&path, root_page(&path, "settings"));
    let mut fresh = Connection::open_in_memory().unwrap();
    populate(&mut fresh, &[], true).unwrap();

    replace(&path, &fresh).unwrap();

    // What the file holds the instant it is at the path: schema and refusals, no -wal needed.
    assert!(!beside(&path, ".new").exists());
    assert!(!beside(&path, "-wal").exists());
    let conn = Connection::open(&path).unwrap();
    assert!(is_clean(&conn).unwrap());
    assert!(matches!(
        read::<bool>(&conn, VIDEO_CODEC_ENABLED.key),
        Read::Stored(false)
    ));
    assert!(matches!(
        read::<bool>(&conn, CONNECT_RECEIVER_ALWAYS_ON.key),
        Read::Stored(false)
    ));
}

#[test]
fn a_damaged_store_is_copied_aside_byte_for_byte_with_no_companions() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    let table = root_page(&path, "settings");
    ruin_page(&path, table);
    let damaged = std::fs::read(&path).unwrap();

    open(&path).unwrap();

    let [aside] = asides(&path).try_into().unwrap();
    assert_eq!(std::fs::read(&aside).unwrap(), damaged);
    assert_eq!(page_image(&aside, table), vec![0xAB; PAGE as usize]);
    assert!(!beside(&aside, "-wal").exists());
    assert!(!beside(&aside, "-shm").exists());
}

#[test]
fn the_copies_of_damaged_stores_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    for _ in 0..(KEPT_ASIDES + 3) {
        ruin_page(&path, root_page(&path, "settings"));
        drop(open(&path).unwrap());
    }
    let before = asides(&path);
    ruin_page(&path, root_page(&path, "settings"));
    drop(open(&path).unwrap());

    let after = asides(&path);
    assert_eq!(after.len(), KEPT_ASIDES);
    assert!(
        after.iter().any(|aside| !before.contains(aside)),
        "the copy just made was the one dropped, not the oldest"
    );
}

#[test]
fn a_vanished_settings_table_is_not_papered_over_before_the_decision() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA writable_schema = ON;
             DELETE FROM sqlite_schema WHERE tbl_name = 'settings';",
        )
        .unwrap();
    }
    block(&beside(&path, ".new"));

    // The run that cannot repair the file ends in memory, and writes nothing into it.
    let (mut conn, opened) = open(&path).unwrap();
    assert!(matches!(opened, Opened::Volatile { .. }));
    assert!(!load_video_codec_enabled(&mut conn));
    drop(conn);
    let probe = Connection::open(&path).unwrap();
    assert!(table_vanished(&probe, &SETTINGS).unwrap());
    drop(probe);

    // The next launch still knows what was lost.
    std::fs::remove_dir_all(beside(&path, ".new")).unwrap();
    let (mut conn, opened) = open(&path).unwrap();
    assert_eq!(
        opened,
        Opened::Rebuilt {
            turned_off: BOTH_CONSENTS.to_vec(),
            trust: Trust::Dropped
        }
    );
    assert!(!load_video_codec_enabled(&mut conn));
    assert!(!load_receiver_always_on(&mut conn));
}

#[test]
fn a_reindex_whose_markers_cannot_be_written_repairs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = table_page_reverted_index_kept(dir.path(), |conn| {
        // The marker cannot be written: what a full disk does to the second statement.
        conn.execute_batch(
            "CREATE TRIGGER no_marker BEFORE INSERT ON settings
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )
        .unwrap();
    });

    assert!(open(&path).is_err());

    // The index is still the damaged one: the next launch finds the same loss, not a repaired
    // file with nothing to say.
    let conn = Connection::open(&path).unwrap();
    assert!(!is_clean(&conn).unwrap());
    assert!(!matches!(
        read::<bool>(&conn, VIDEO_CODEC_ENABLED.key),
        Read::Stored(_)
    ));
}

/// A profile whose first row is one the table keeps but cannot be read as text (`first`), then
/// the two refusals, with the trust table ruined: the settings table passes its own check.
fn unreadable_row_first(dir: &Path, first: &str) -> PathBuf {
    let path = dir.join("settings.db");
    let mut conn = Connection::open(&path).unwrap();
    init_schema(&mut conn).unwrap();
    conn.execute(first, []).unwrap();
    save_video_codec_enabled(&mut conn, false);
    save_receiver_always_on(&mut conn, false);
    insert_refused_trust(&conn);
    drop(conn);
    ruin_page(&path, root_page(&path, "native_trust"));
    path
}

#[test]
fn a_row_the_scan_cannot_read_does_not_turn_the_refusals_after_it_into_consents() {
    for first in [
        "INSERT INTO settings (key, value) VALUES ('window.width', x'31323030')",
        "INSERT INTO settings (key, value) VALUES ('window.width', CAST(x'FFFEFFFE' AS TEXT))",
        "INSERT INTO settings (key, value) VALUES (NULL, '1200')",
        "INSERT INTO settings (key, value) VALUES (x'61626364', '1200')",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = unreadable_row_first(dir.path(), first);
        // The structural check is blind to it: only the copy can tell.
        assert!(settings_clean(&Connection::open(&path).unwrap()), "{first}");

        let (mut conn, opened) = open(&path).unwrap();

        assert_eq!(
            opened,
            Opened::Rebuilt {
                turned_off: vec![],
                trust: Trust::Unknown
            },
            "{first}"
        );
        assert!(!load_video_codec_enabled(&mut conn), "{first}");
        assert!(!load_receiver_always_on(&mut conn), "{first}");
    }
}

#[test]
fn a_refusal_stored_unreadably_reads_refused_after_a_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE settings SET value = x'66616c7365' WHERE key = ?1",
            [VIDEO_CODEC_ENABLED.key],
        )
        .unwrap();
    }
    ruin_page(&path, root_page(&path, "native_trust"));

    let (mut conn, opened) = open(&path).unwrap();

    assert_eq!(
        opened,
        Opened::Rebuilt {
            turned_off: vec![VIDEO_CODEC_ENABLED.key],
            trust: Trust::Unknown
        }
    );
    assert!(!load_video_codec_enabled(&mut conn));
    assert!(!load_receiver_always_on(&mut conn));
}

/// A skipped row ends nothing: a choice stored after it still carries over.
#[test]
fn a_consent_the_user_gave_survives_an_unreadable_row_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.db");
    {
        let mut conn = Connection::open(&path).unwrap();
        init_schema(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('window.width', x'31323030')",
            [],
        )
        .unwrap();
        save_video_codec_enabled(&mut conn, true);
        insert_refused_trust(&conn);
    }
    ruin_page(&path, root_page(&path, "native_trust"));

    let (mut conn, opened) = open(&path).unwrap();

    assert!(matches!(opened, Opened::Rebuilt { .. }));
    assert!(load_video_codec_enabled(&mut conn));
}

#[test]
fn a_scan_that_cannot_run_or_stops_early_is_not_intact() {
    let other_columns = Connection::open_in_memory().unwrap();
    other_columns
        .execute_batch("CREATE TABLE settings (k TEXT PRIMARY KEY, v TEXT NOT NULL)")
        .unwrap();
    assert!(!salvage(&other_columns).intact);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.db");
    {
        let mut conn = Connection::open(&path).unwrap();
        init_schema(&mut conn).unwrap();
        for n in 0..300 {
            set(&conn, &format!("filler.{n:03}"), &"x".repeat(60));
        }
    }
    assert!(salvage(&Connection::open(&path).unwrap()).intact);

    // The table now spans several leaves under an interior root: ruin the second child.
    let root = root_page(&path, "settings");
    let image = page_image(&path, root);
    let cell = u16::from_be_bytes([image[14], image[15]]) as usize;
    let child = u32::from_be_bytes(image[cell..cell + 4].try_into().unwrap());
    ruin_page(&path, u64::from(child));

    let salvaged = salvage(&Connection::open(&path).unwrap());
    assert!(!salvaged.intact);
    assert!(!salvaged.rows.is_empty());
}

/// A profile that refused the two consents and holds no trust decision.
fn seeded_without_trust(dir: &Path) -> PathBuf {
    let path = dir.join("settings.db");
    let mut conn = Connection::open(&path).unwrap();
    init_schema(&mut conn).unwrap();
    save_video_codec_enabled(&mut conn, false);
    save_receiver_always_on(&mut conn, false);
    path
}

#[test]
fn a_rebuild_that_lost_no_trust_decision_does_not_announce_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded_without_trust(dir.path());
    ruin_page(&path, root_page(&path, "settings"));

    let (_, opened) = open(&path).unwrap();

    assert_eq!(
        opened,
        Opened::Rebuilt {
            turned_off: BOTH_CONSENTS.to_vec(),
            trust: Trust::Empty
        }
    );
}

#[test]
fn the_trust_probe_reads_both_trees() {
    let dir = tempfile::tempdir().unwrap();
    let empty = seeded_without_trust(dir.path());
    assert_eq!(
        trust_at_open(&Connection::open(&empty).unwrap()),
        Trust::Empty
    );

    let other = tempfile::tempdir().unwrap();
    let held = seeded(other.path());
    assert_eq!(
        trust_at_open(&Connection::open(&held).unwrap()),
        Trust::Dropped
    );

    // A lost write that left the table root as a valid empty leaf, the index still listing the
    // row: a count of the table alone would answer nothing to lose.
    let mut leaf = vec![0u8; PAGE as usize];
    leaf[0] = 0x0D;
    leaf[5..7].copy_from_slice(&(PAGE as u16).to_be_bytes());
    let table = root_page(&held, "native_trust");
    overwrite(&held, (table - 1) * PAGE, &leaf);
    assert_eq!(
        trust_at_open(&Connection::open(&held).unwrap()),
        Trust::Dropped
    );

    let garbage = tempfile::tempdir().unwrap();
    let not_a_db = garbage.path().join("settings.db");
    std::fs::write(&not_a_db, b"this is not a database header, nor a page").unwrap();
    assert_eq!(
        trust_at_open(&Connection::open(&not_a_db).unwrap()),
        Trust::Unknown
    );
}

/// A damaged store in a directory named `name`: rebuilt on disk, the refusals kept, a copy aside.
fn rebuilt_in_a_directory_named(name: &std::ffi::OsStr) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join(name);
    std::fs::create_dir(&dir).unwrap();
    let path = seeded(&dir);
    ruin_page(&path, root_page(&path, "settings"));

    let (mut conn, opened) = open(&path).unwrap();

    assert_eq!(
        opened,
        Opened::Rebuilt {
            turned_off: BOTH_CONSENTS.to_vec(),
            trust: Trust::Dropped
        }
    );
    assert_eq!(asides(&path).len(), 1);
    assert!(leftovers(&path).is_empty());
    assert!(!load_video_codec_enabled(&mut conn));
    assert!(!load_receiver_always_on(&mut conn));
    drop(conn);
    assert_eq!(open(&path).unwrap().1, Opened::Healthy);
}

/// Names a directory can hold on every OS, among them the characters a SQLite URI reads as syntax
/// (`#` starts a fragment, `%` an escape).
#[test]
fn a_directory_name_that_is_special_to_a_uri_does_not_change_where_the_store_goes() {
    for name in ["a b", "a#b", "a%41b", "100%", "\u{e9}t\u{e9}"] {
        rebuilt_in_a_directory_named(std::ffi::OsStr::new(name));
    }
}

/// The rest of the URI syntax (`?` starts the query, `file:` is the scheme) is a valid directory
/// name on Unix only: Windows refuses both characters, and no such directory can exist there.
#[cfg(unix)]
#[test]
fn a_directory_name_that_is_a_uri_query_or_scheme_does_not_change_where_the_store_goes() {
    for name in ["a?b=c", "file:x", "x?mode=ro&nolock=1"] {
        rebuilt_in_a_directory_named(std::ffi::OsStr::new(name));
    }
}

/// Linux only: APFS refuses to create a name that is not UTF-8 (EILSEQ) and exFAT does too, while
/// HFS+ and FAT store it as `%XX`; on macOS the directory built here is not the one named.
#[cfg(target_os = "linux")]
#[test]
fn a_damaged_store_in_a_directory_that_is_not_utf8_is_rebuilt_on_disk() {
    use std::os::unix::ffi::OsStrExt;
    for name in [&b"d\xff"[..], b"\xe9t\xe9", b"d\xff?#%41 x"] {
        rebuilt_in_a_directory_named(std::ffi::OsStr::from_bytes(name));
    }
}

fn delete_schema_rows(path: &Path, table: &str) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch("PRAGMA writable_schema = ON;").unwrap();
    conn.execute("DELETE FROM sqlite_schema WHERE tbl_name = ?1", [table])
        .unwrap();
}

fn set_notice_row(path: &Path, json: &str) {
    let conn = Connection::open(path).unwrap();
    conn.execute("UPDATE repair_notice SET notice = ?1", [json])
        .unwrap();
}

/// A profile whose first repair was never told of: both consents turned off, trust dropped.
fn repaired_untold(dir: &Path) -> PathBuf {
    let path = seeded(dir);
    ruin_page(&path, root_page(&path, "settings"));
    drop(open(&path).unwrap());
    path
}

/// What a notice that cannot be read again is stood in for by: every setting reading as a repair
/// leaves it, and permissions that may be asked again.
fn the_wider_notice() -> Option<RepairNotice> {
    told_of(&BOTH_CONSENTS, Trust::Unknown, true)
}

#[test]
fn an_earlier_notice_that_cannot_be_read_is_told_as_a_wider_loss_by_the_next_repair() {
    let damages: [(&str, Damage); 2] = [
        ("a ruined page", |path| {
            ruin_page(path, root_page(path, "repair_notice"))
        }),
        ("a table gone from the schema", |path| {
            delete_schema_rows(path, "repair_notice")
        }),
    ];
    for (what, damage) in damages {
        let dir = tempfile::tempdir().unwrap();
        let path = repaired_untold(dir.path());
        damage(&path);

        let (conn, opened) = open(&path).unwrap();

        assert!(matches!(opened, Opened::Rebuilt { .. }), "{what}");
        assert_eq!(pending_notice(&conn), the_wider_notice(), "{what}");
    }
}

#[test]
fn a_notice_row_that_cannot_be_parsed_is_still_owed_as_a_wider_loss() {
    for json in [
        "{not json",
        r#"{"turnedOff":["video.codec_enabled"],"trust":"partial","saved":true}"#,
        r#"{"turnedOff":["video.codec_enabled"],"trust":"dropped"}"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = repaired_untold(dir.path());
        set_notice_row(&path, json);

        let (conn, opened) = open(&path).unwrap();

        assert_eq!(opened, Opened::Healthy, "{json}");
        assert_eq!(pending_notice(&conn), the_wider_notice(), "{json}");
        acknowledge_repair_notice(&conn);
        assert_eq!(pending_notice(&conn), None, "{json}");
    }
}

#[test]
fn a_notice_row_from_a_later_build_keeps_what_this_build_can_tell() {
    let dir = tempfile::tempdir().unwrap();
    let path = repaired_untold(dir.path());
    set_notice_row(
        &path,
        r#"{"turnedOff":["video.codec_enabled","future.key"],"trust":"dropped","saved":true,"extra":1}"#,
    );

    let (conn, _) = open(&path).unwrap();

    assert_eq!(
        pending_notice(&conn),
        told_of(&[VIDEO_CODEC_ENABLED.key], Trust::Dropped, true)
    );
}

#[test]
fn a_reindex_replaces_an_unreadable_earlier_notice_instead_of_leaving_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = repaired_untold(dir.path());
    set_notice_row(&path, "{not json");
    corrupt_index_byte(&path);

    let (conn, opened) = open(&path).unwrap();

    assert_eq!(
        opened,
        Opened::Reindexed {
            turned_off: vec![],
            trust: Trust::Empty
        }
    );
    assert_eq!(pending_notice(&conn), the_wider_notice());
    assert!(read_notice(&conn).is_ok());
}

/// A trust table whose page went back to before a refusal while its index kept it: a write that
/// never reached the disk. `seed` runs on the connection that created the schema.
fn trust_page_reverted_index_kept(dir: &Path, seed: impl FnOnce(&mut Connection)) -> PathBuf {
    let path = dir.join("settings.db");
    {
        let mut conn = Connection::open(&path).unwrap();
        init_schema(&mut conn).unwrap();
        seed(&mut conn);
    }
    let table = root_page(&path, "native_trust");
    let before = page_image(&path, table);
    insert_refused_trust(&Connection::open(&path).unwrap());
    overwrite(&path, (table - 1) * PAGE, &before);
    path
}

#[test]
fn a_trust_decision_the_table_lost_is_told_by_a_reindex() {
    type Seed = fn(&mut Connection);
    let cases: [(&str, Seed, bool, &[&'static str]); 2] = [
        (
            "a reindex alone",
            |conn| save_log_level(conn, 2),
            false,
            &[],
        ),
        (
            "a reindex that also turned a setting off",
            |conn| {
                save_video_codec_enabled(conn, false);
                save_log_level(conn, 2);
            },
            true,
            &[CONNECT_RECEIVER_ALWAYS_ON.key],
        ),
    ];
    for (what, seed, damage_index, turned_off) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = trust_page_reverted_index_kept(dir.path(), seed);
        if damage_index {
            corrupt_index_byte(&path);
        }

        let (conn, opened) = open(&path).unwrap();

        assert_eq!(
            opened,
            Opened::Reindexed {
                turned_off: turned_off.to_vec(),
                trust: Trust::Dropped
            },
            "{what}"
        );
        assert_eq!(
            pending_notice(&conn),
            told_of(turned_off, Trust::Dropped, true),
            "{what}"
        );
        assert_eq!(trust_rows(&conn), 0, "{what}");
    }
}

#[test]
fn the_reindex_probe_tells_what_the_index_lists_beyond_the_table() {
    let probe = |path: &Path| trust_lost_to_reindex(&Connection::open(path).unwrap());
    let overwrite_root = |path: &Path, name: &str, kind: u8| {
        let mut leaf = vec![0u8; PAGE as usize];
        leaf[0] = kind;
        leaf[5..7].copy_from_slice(&(PAGE as u16).to_be_bytes());
        overwrite(path, (root_page(path, name) - 1) * PAGE, &leaf);
    };

    let empty = tempfile::tempdir().unwrap();
    assert_eq!(probe(&seeded_without_trust(empty.path())), Trust::Empty);

    // Both trees hold the row: REINDEX keeps it.
    let other = tempfile::tempdir().unwrap();
    let held = seeded(other.path());
    assert_eq!(probe(&held), Trust::Empty);

    // The index lost the row and the table did not: rebuilt from the table, nothing is gone.
    overwrite_root(&held, "sqlite_autoindex_native_trust_1", 0x0A);
    assert_eq!(probe(&held), Trust::Empty);

    // The table lost the row and the index did not.
    let third = tempfile::tempdir().unwrap();
    let stale = seeded(third.path());
    overwrite_root(&stale, "native_trust", 0x0D);
    assert_eq!(probe(&stale), Trust::Dropped);

    let garbage = tempfile::tempdir().unwrap();
    let not_a_db = garbage.path().join("settings.db");
    std::fs::write(&not_a_db, b"this is not a database header, nor a page").unwrap();
    assert_eq!(probe(&not_a_db), Trust::Unknown);
}

/// The schema every published build wrote: no repair notice table, and no stamp.
const RELEASED_SCHEMA: &str = "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
     CREATE TABLE native_trust (
        code_hash TEXT NOT NULL, plugin TEXT NOT NULL, module TEXT NOT NULL,
        granted INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (code_hash, plugin, module));";

/// A store as a published build left it, with a log level and the video refusal, plus `rows`.
fn released(dir: &Path, rows: &str) -> PathBuf {
    let path = dir.join("settings.db");
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(RELEASED_SCHEMA).unwrap();
    conn.execute_batch(
        "INSERT INTO settings VALUES ('logging.level', '2');
         INSERT INTO settings VALUES ('video.codec_enabled', 'false');",
    )
    .unwrap();
    conn.execute_batch(rows).unwrap();
    path
}

#[test]
fn a_damaged_store_from_before_the_notice_is_told_only_what_its_own_repair_did() {
    let receiver_off = "INSERT INTO settings VALUES ('connect.receiver_always_on', 'false');";
    // (rows the user left, damage, what the repair itself turned off)
    let cases = [
        (receiver_off, corrupt_index_byte as fn(&Path), vec![]),
        ("", corrupt_index_byte, vec![CONNECT_RECEIVER_ALWAYS_ON.key]),
        (
            receiver_off,
            |path: &Path| ruin_page(path, root_page(path, "settings")),
            BOTH_CONSENTS.to_vec(),
        ),
    ];
    for (rows, damage, turned_off) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = released(dir.path(), rows);
        damage(&path);

        let (conn, opened) = open(&path).unwrap();

        // No earlier notice existed to be lost: the user is told this repair and nothing wider.
        assert_eq!(opened.notice(), pending_notice(&conn), "{rows}");
        let told = pending_notice(&conn);
        assert_eq!(
            told.as_ref().map(|notice| notice.turned_off.clone()),
            (!turned_off.is_empty()).then_some(turned_off.clone()),
            "{rows}"
        );
        assert!(
            told.is_none_or(|notice| notice.trust == Trust::Empty),
            "{rows}"
        );
    }
}

#[test]
fn a_store_is_stamped_with_the_tables_it_holds() {
    let dir = tempfile::tempdir().unwrap();

    let mut conn = Connection::open(dir.path().join("fresh.db")).unwrap();
    init_schema(&mut conn).unwrap();
    assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);

    // A published store is stamped as it gains the table the stamp vouches for.
    let before = tempfile::tempdir().unwrap();
    let old = released(before.path(), "");
    let (conn, opened) = open(&old).unwrap();
    assert_eq!(opened, Opened::Healthy);
    assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
    assert_eq!(pending_notice(&conn), None);

    // A later build's stamp is never lowered.
    conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    let mut conn = Connection::open(&old).unwrap();
    init_schema(&mut conn).unwrap();
    assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION + 1);
}

#[test]
fn the_stamp_and_its_tables_land_together_or_not_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let path = released(dir.path(), "");
    let mut conn = Connection::open(&path).unwrap();
    let pages: i64 = conn
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap();
    // No room for the table the stamp vouches for.
    conn.pragma_update(None, "max_page_count", pages).unwrap();

    assert!(init_schema(&mut conn).is_err());

    assert_eq!(schema_version(&conn).unwrap(), 0);
    assert!(!table_exists(&conn, &REPAIR_NOTICE));
}

fn table_exists(conn: &Connection, table: &Table) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name = ?1",
        [table.name],
        |row| row.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

#[test]
fn a_table_is_missed_from_a_store_that_had_it_and_not_from_one_that_predates_it() {
    for table in TABLES {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.db");
        let mut conn = Connection::open(&path).unwrap();
        init_schema(&mut conn).unwrap();
        drop(conn);
        delete_schema_rows(&path, table.name);

        let conn = Connection::open(&path).unwrap();
        assert!(table_vanished(&conn, table).unwrap(), "{}", table.name);

        // Stamped before the table was added, it never had it.
        if table.since > 0 {
            conn.pragma_update(None, "user_version", table.since - 1)
                .unwrap();
            assert!(!table_vanished(&conn, table).unwrap(), "{}", table.name);
        }
    }
}

#[test]
fn a_launch_on_a_stamped_store_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = seeded(dir.path());
    let mut conn = Connection::open(&path).unwrap();

    init_schema(&mut conn).unwrap();

    // The -wal holds every write since the last checkpoint.
    let wal = std::fs::metadata(beside(&path, "-wal")).map_or(0, |wal| wal.len());
    assert_eq!(wal, 0);
}
