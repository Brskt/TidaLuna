//! Tests for `src/native_runtime/trust.rs`, attached to it by `#[path]`: which rows a trust
//! revocation reaches, and what it waits for.

use super::{clear_all_trust, clear_trust_by_plugin, load_trust, save_trust, save_trust_answers};
use crate::plugins::name::PluginName;
use crate::settings::durability_settings_tests::{
    after_a_power_cut, assert_in_order, statements_of, syncs_fully, syncs_routinely,
};
use crate::settings::fixture::{file_conn, mem_conn};
use rusqlite::Connection;
use std::path::Path;

const PLUGIN: &str = "net/a.native.ts";

fn net() -> PluginName {
    PluginName::parse("net").unwrap()
}

fn granted(conn: &mut Connection) -> Vec<bool> {
    load_trust(conn, PLUGIN, "h")
        .unwrap()
        .into_iter()
        .map(|d| d.granted)
        .collect()
}

#[test]
fn clearing_a_plugins_trust_is_written_under_a_full_sync() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = file_conn(dir.path());
    save_trust(&conn, "h", PLUGIN, "net", true).unwrap();
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
    save_trust(&conn, "h", PLUGIN, "net", true).unwrap();
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    clear_trust_by_plugin(&mut conn, &net()).unwrap();
    let cleared = wal_len(dir.path());
    save_trust(&conn, "h", PLUGIN, "net", false).unwrap();
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

/// Bun honours grants module by module: an answer cut short would leave part of a refused row
/// granted. Wherever the log is cut, the answer is stored whole or not at all.
#[test]
fn a_trust_answer_is_stored_whole_or_not_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = file_conn(dir.path());
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    let modules = [
        "net",
        "http",
        "https",
        "http2",
        "tls",
        "dns",
        "dns/promises",
    ];
    let answer: Vec<(&str, bool)> = modules.iter().map(|m| (*m, false)).collect();
    save_trust_answers(&mut conn, "h", PLUGIN, &answer).unwrap();
    let written = wal_len(dir.path());
    let page: i64 = conn
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .unwrap();

    // A log is a 32-byte header, then frames of a 24-byte header and one page each.
    let cuts = std::iter::once(0).chain((32..=written).step_by(24 + page as usize));
    for kept in cuts {
        let cut = after_a_power_cut(dir.path(), "settings.db", kept);
        let stored: i64 = cut
            .query_row("SELECT COUNT(*) FROM native_trust", [], |row| row.get(0))
            .unwrap();
        assert!(
            stored == 0 || stored == modules.len() as i64,
            "{stored} modules stored after a cut at byte {kept}"
        );
    }
}

fn modules(conn: &mut Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT plugin FROM native_trust ORDER BY plugin")
        .unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn seeded(modules: &[&str]) -> Connection {
    let conn = mem_conn();
    for module in modules {
        save_trust(&conn, "h", module, "fs", true).unwrap();
    }
    conn
}

fn clear(conn: &mut Connection, name: &str) {
    clear_trust_by_plugin(conn, &PluginName::parse(name).unwrap()).unwrap();
}

/// `_` is a legal plugin-name character and a wildcard in SQL `LIKE`, which also ignores ASCII
/// case: uninstalling `my_plugin` used to revoke what the user had granted `myXplugin`, and `Foo`
/// and `foo` are two plugins even where the file system cannot tell their folders apart.
#[test]
fn clearing_a_plugin_revokes_only_its_own_modules() {
    let cases: [(&[&str], &str, &[&str]); 3] = [
        (
            &["my_plugin/x.native.ts", "myXplugin/x.native.ts"],
            "my_plugin",
            &["myXplugin/x.native.ts"],
        ),
        (
            &["Foo", "Foo/x.native.ts", "foo", "foo/x.native.ts"],
            "Foo",
            &["foo", "foo/x.native.ts"],
        ),
        (
            &["@s/p", "@s/p/x.native.ts", "@s/pq/x.native.ts"],
            "@s/p",
            &["@s/pq/x.native.ts"],
        ),
    ];
    for (seed, plugin, left) in cases {
        let mut conn = seeded(seed);
        clear(&mut conn, plugin);
        assert_eq!(
            modules(&mut conn),
            left,
            "clearing {plugin:?} from {seed:?}"
        );
    }
}

/// One `save_trust` of module `fs`: code hash, plugin, granted.
type Saved = (&'static str, &'static str, bool);

/// An update must not inherit what the user allowed an older native file, nor shed what the user
/// refused it, nor win a grant back by serving the granted file again; an older refusal does not
/// outlive a later grant.
#[test]
fn a_code_change_keeps_the_latest_refusal_but_no_grant() {
    // Saved in order, then the `fs` decision `P` reads at hash "new".
    let cases: [(&[Saved], Option<bool>); 6] = [
        (&[("old", "P", false)], Some(false)),
        (&[("old", "P", true)], None),
        (&[("h0", "P", false), ("h1", "P", true)], None),
        (&[("h0", "P", true), ("h1", "P", false)], Some(false)),
        (&[("old", "P", false), ("new", "P", true)], Some(true)),
        (&[("new", "P", true), ("old", "P", false)], Some(false)),
    ];
    for (history, expected) in cases {
        let mut conn = mem_conn();
        for (hash, plugin, granted) in history {
            save_trust(&conn, hash, plugin, "fs", *granted).unwrap();
        }
        let read: Vec<(String, bool)> = load_trust(&mut conn, "P", "new")
            .unwrap()
            .into_iter()
            .map(|d| (d.module, d.granted))
            .collect();
        let expected: Vec<(String, bool)> =
            expected.map(|g| ("fs".into(), g)).into_iter().collect();
        assert_eq!(read, expected, "history {history:?}");
    }
}

/// `plugin.uninstall_all` clears everything by its own statement, not by a name that happens to
/// match every row.
#[test]
fn clearing_all_trust_leaves_nothing() {
    let mut conn = seeded(&["a/x.native.ts", "B", "@s/p/x.native.ts"]);
    clear_all_trust(&mut conn).unwrap();
    assert!(modules(&mut conn).is_empty());
}
