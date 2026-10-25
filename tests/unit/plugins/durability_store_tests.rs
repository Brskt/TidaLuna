//! Tests for what a plugin's switch-off or removal waits for, in `src/plugins/store.rs`, attached by
//! `#[path]`.

use super::*;
use crate::settings::durability_settings_tests::{
    after_a_power_cut, assert_in_order, statements_of, syncs_fully, syncs_routinely,
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
    let name = crate::plugins::name::PluginName::parse(name).unwrap();
    install(conn, url, &name, "{}", "code", "hash").unwrap();
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
