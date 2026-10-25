//! Tests for what a trust revocation waits for, in `src/native_runtime/trust.rs`, attached to
//! `src/settings.rs` by `#[path]`.

use super::durability_settings_tests::{
    after_a_power_cut, assert_in_order, statements_of, syncs_fully, syncs_routinely,
};
use super::fixture::file_conn;
use super::*;
use crate::native_runtime::trust::{clear_trust_by_plugin, load_trust, save_trust};
use crate::plugins::name::PluginName;
use std::path::Path;

const PLUGIN: &str = "net/a.native.ts";

fn net() -> PluginName {
    PluginName::parse("net").unwrap()
}

fn granted(conn: &mut Connection) -> Vec<bool> {
    load_trust(conn, PLUGIN, "h")
        .into_iter()
        .map(|d| d.granted)
        .collect()
}

#[test]
fn clearing_a_plugins_trust_is_written_under_a_full_sync() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = file_conn(dir.path());
    save_trust(&mut conn, "h", PLUGIN, "net", true).unwrap();
    let statements = statements_of(&mut conn, |c| clear_trust_by_plugin(c, &net()).unwrap());
    assert_in_order(
        &statements,
        &[
            &syncs_fully,
            &|sql| sql.starts_with("delete from native_trust"),
            &syncs_routinely,
        ],
    );
    assert!(granted(&mut conn).is_empty());
}

fn wal_len(dir: &Path) -> u64 {
    std::fs::metadata(dir.join("settings.db-wal"))
        .unwrap()
        .len()
}

#[test]
fn a_refusal_lost_with_the_tail_asks_again_once_the_grant_is_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = file_conn(dir.path());
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    save_trust(&mut conn, "h", PLUGIN, "net", true).unwrap();
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    clear_trust_by_plugin(&mut conn, &net()).unwrap();
    let cleared = wal_len(dir.path());
    save_trust(&mut conn, "h", PLUGIN, "net", false).unwrap();
    let refused = wal_len(dir.path());
    assert!(cleared > 0 && refused > cleared);

    let mut lost_all = after_a_power_cut(dir.path(), "settings.db", 0);
    assert_eq!(
        granted(&mut lost_all),
        [true],
        "a clear lost with the tail brings the old grant back"
    );
    let mut lost_refusal = after_a_power_cut(dir.path(), "settings.db", cleared);
    assert!(granted(&mut lost_refusal).is_empty());
    let mut lost_nothing = after_a_power_cut(dir.path(), "settings.db", refused);
    assert_eq!(granted(&mut lost_nothing), [false]);
}
