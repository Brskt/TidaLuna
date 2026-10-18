//! Tests for what a plugin's switch-off or removal waits for, in `src/plugins/store.rs`, attached by
//! `#[path]`.

use super::*;
use crate::db::Stable;
use crate::settings::durability_settings_tests::{
    after_a_power_cut, assert_in_order, position, statements_of, syncs_fully, syncs_routinely,
};
use std::path::Path;

const URL: &str = "https://plugins.example/a";

/// A database in a file, with a log like the app's: an in-memory one has nothing to sync.
fn file_conn(dir: &Path) -> Connection {
    let mut conn = Connection::open(dir.join("plugins.db")).unwrap();
    init_schema(&mut conn).unwrap();
    conn
}

fn seed(conn: &mut Connection, url: &str, name: &str) {
    install(conn, url, name, "{}", "code", "hash").unwrap();
    enable(conn, url).unwrap();
    storage_set(conn, url, "k", "v").unwrap();
}

/// How many plugin rows and storage rows `conn` holds.
fn rows(conn: &Connection) -> (i64, i64) {
    let count = |table: &str| {
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    };
    (count("plugins"), count("plugin_storage"))
}

fn sync_level(conn: &Connection) -> (i64, i64) {
    let pragma = |name: &str| {
        conn.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
            .unwrap()
    };
    (pragma("synchronous"), pragma("fullfsync"))
}

/// A second transaction would pass `assert_in_order`, which only finds the first of each.
fn assert_one_transaction(statements: &[String]) {
    for keyword in ["begin", "commit"] {
        let count = statements
            .iter()
            .filter(|sql| sql.starts_with(keyword))
            .count();
        assert_eq!(count, 1, "{keyword} ran {count} times: {statements:?}");
    }
}

#[test]
fn disabling_a_plugin_is_written_under_a_full_sync() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = file_conn(dir.path());
    seed(&mut conn, URL, "a");
    let statements = statements_of(&mut conn, |c| disable(c, URL).unwrap());
    assert_in_order(
        &statements,
        &[
            &syncs_fully,
            &|sql| sql.starts_with("update plugins set enabled = 0"),
            &syncs_routinely,
        ],
    );
    assert!(collect_enabled_code(&mut conn).is_empty());
}

#[test]
fn uninstalling_a_plugin_is_one_commit_under_a_full_sync() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = file_conn(dir.path());
    seed(&mut conn, URL, "a");
    let statements = statements_of(&mut conn, |c| uninstall(c, URL).unwrap());
    assert_in_order(
        &statements,
        &[
            &syncs_fully,
            &|sql| sql.starts_with("begin"),
            &|sql| sql.starts_with("delete from plugins where"),
            &|sql| sql.starts_with("delete from plugin_storage"),
            &|sql| sql.starts_with("commit"),
            &syncs_routinely,
        ],
    );
    assert_one_transaction(&statements);
    assert_eq!(rows(&conn), (0, 0));
}

#[test]
fn uninstalling_every_plugin_is_one_commit_under_a_full_sync() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = file_conn(dir.path());
    seed(&mut conn, URL, "a");
    seed(&mut conn, "https://plugins.example/b", "b");
    let statements = statements_of(&mut conn, |c| uninstall_all(c).unwrap());
    assert_in_order(
        &statements,
        &[
            &syncs_fully,
            &|sql| sql.starts_with("begin"),
            &|sql| sql.starts_with("delete from plugins"),
            &|sql| sql.starts_with("delete from plugin_storage"),
            &|sql| sql.starts_with("commit"),
            &syncs_routinely,
        ],
    );
    assert_one_transaction(&statements);
    assert_eq!(rows(&conn), (0, 0));
}

#[test]
fn a_failed_load_is_switched_off_on_the_routine_sync() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = file_conn(dir.path());
    seed(&mut conn, URL, "a");
    let statements = statements_of(&mut conn, |c| disable_failed(c, URL).unwrap());
    assert!(
        position(&statements, |sql| sql.contains("synchronous")).is_none(),
        "a flag a power cut brings back only retries the load: {statements:?}"
    );
    assert!(collect_enabled_code(&mut conn).is_empty());
}

#[test]
fn a_startup_dedup_removes_its_losers_on_the_routine_sync() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = file_conn(dir.path());
    seed(&mut conn, URL, "same");
    conn.execute(
        "INSERT INTO plugins (url, name, manifest, code, hash, enabled, installed)
         VALUES ('https://plugins.example/b', 'same', '{}', 'code', 'hash', 0, 1)",
        [],
    )
    .unwrap();
    let mut removed = Vec::new();
    let statements = statements_of(&mut conn, |c| removed.extend(dedup_same_name(c)));
    assert_eq!(removed.len(), 1);
    assert!(
        position(&statements, |sql| sql.contains("synchronous")).is_none(),
        "a removal found again at the next launch must not pay a sync: {statements:?}"
    );
}

#[test]
fn the_connection_goes_back_to_the_routine_sync_after_a_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = file_conn(dir.path());
    seed(&mut conn, URL, "a");
    disable(&mut conn, URL).unwrap();
    assert_eq!(sync_level(&conn), (1, 0));
    uninstall(&mut conn, URL).unwrap();
    assert_eq!(sync_level(&conn), (1, 0));
    uninstall_all(&mut conn).unwrap();
    assert_eq!(sync_level(&conn), (1, 0));
}

#[test]
fn a_guard_inside_a_transaction_names_the_store_it_could_not_sync() {
    let dir = tempfile::tempdir().unwrap();
    let conn = file_conn(dir.path());
    crate::logging::take_lines();
    {
        let tx = conn.unchecked_transaction().unwrap();
        let _stable = Stable::begin(&tx);
    }
    let lines = crate::logging::take_lines();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("plugins.db") && line.contains("cannot sync")),
        "{lines:?}"
    );
    assert_eq!(sync_level(&conn), (1, 0));
}

#[test]
fn a_power_cut_never_leaves_a_plugin_half_removed() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = file_conn(dir.path());
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    seed(&mut conn, URL, "a");
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    uninstall(&mut conn, URL).unwrap();

    let wal_len = std::fs::metadata(dir.path().join("plugins.db-wal"))
        .unwrap()
        .len();
    let page_size: i64 = conn
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .unwrap();
    // Cuts are tried on the log's frame boundaries (a 32-byte header, then 24 + one page each).
    let frame = 24 + page_size as u64;
    let cuts: Vec<u64> = std::iter::once(0)
        .chain(
            (0..)
                .map(|n| 32 + n * frame)
                .take_while(|&end| end <= wal_len),
        )
        .collect();
    assert!(cuts.len() > 2, "the removal wrote no frame: {wal_len}");
    for kept in cuts {
        let cut = after_a_power_cut(dir.path(), "plugins.db", kept);
        let state = rows(&cut);
        assert!(
            state == (1, 1) || state == (0, 0),
            "a cut at {kept} of {wal_len} bytes left a half-removed plugin: {state:?}"
        );
    }
    assert_eq!(
        rows(&after_a_power_cut(dir.path(), "plugins.db", 0)),
        (1, 1)
    );
    assert_eq!(
        rows(&after_a_power_cut(dir.path(), "plugins.db", wal_len)),
        (0, 0)
    );
}
