//! Tests for `src/plugins/store/mod.rs`, attached to it by `#[path]`.

use super::*;
use crate::plugins::name::PluginName;
use crate::settings::fixture::{
    after_a_power_cut, assert_in_order, statements_of, syncs_fully, syncs_routinely,
};
use std::path::Path;

pub(super) fn mem() -> Connection {
    let mut conn = Connection::open_in_memory().expect("open in-memory db");
    init_schema(&mut conn).expect("init schema");
    conn
}

fn name(raw: &str) -> PluginName {
    PluginName::parse(raw).unwrap()
}

/// A row as an older build could have written it, name unchecked.
pub(super) fn stored_row(conn: &mut Connection, url: &str, raw_name: &str, manifest: &str) {
    conn.execute(
        "INSERT INTO plugins (url, name, manifest, code, hash, enabled, installed)
         VALUES (?1, ?2, ?3, 'code', 'h', 1, 1)",
        params![url, raw_name, manifest],
    )
    .unwrap();
}

/// Windows and macOS give `Foo` and `foo` one data dir: the second would read and write the
/// first one's files.
#[test]
fn a_name_installed_in_another_case_is_taken() {
    let conn = mem();
    install(&conn, "https://a/Foo", &name("Foo"), "{}", "c", "h").unwrap();
    let refused = install(&conn, "https://b/foo", &name("foo"), "{}", "c", "h");
    assert!(refused.is_err(), "a second plugin named foo was installed");
    install(&conn, "https://a/Foo", &name("Foo"), "{}", "c2", "h2")
        .expect("reinstalling the same url is an update, not a clash");
}

#[test]
fn a_pair_installed_before_the_case_rule_is_found() {
    let mut conn = mem();
    stored_row(&mut conn, "https://a/Foo", "Foo", "{}");
    stored_row(&mut conn, "https://b/foo", "foo", "{}");
    stored_row(&mut conn, "https://c/Bar", "Bar", "{}");
    assert!(case_twin_installed(&mut conn, &name("foo")));
    assert!(case_twin_installed(&mut conn, &name("Foo")));
    assert!(!case_twin_installed(&mut conn, &name("Bar")));
}

/// A plugin disabled for its name or manifest still says why on its card after the restart: the
/// reason is read off the row, not kept from the startup pass.
#[test]
fn a_listed_plugin_carries_why_it_cannot_load() {
    let mut conn = mem();
    stored_row(&mut conn, "https://a/dot", ".", "{}");
    stored_row(&mut conn, "https://b/bad", "Bad", r#"{"luna": 1}"#);
    stored_row(&mut conn, "https://c/ok", "Ok", "{}");
    let listed = list(&mut conn).unwrap();
    let reason = |url: &str| {
        listed
            .iter()
            .find(|p| p.url == url)
            .unwrap()
            .load_error
            .clone()
    };
    assert!(reason("https://a/dot").is_some_and(|r| r.contains("\".\"")));
    assert!(reason("https://b/bad").is_some_and(|r| r.contains("luna")));
    assert_eq!(reason("https://c/ok"), None);
}

/// Whether `url` still has its row and the storage value `seeded` put there.
fn kept(conn: &mut Connection, url: &str) -> bool {
    list(conn).unwrap().iter().any(|p| p.url == url)
        && storage_get(conn, url, "k").as_deref() == Some("v")
}

fn seeded(conn: &mut Connection, url: &str, raw_name: &str, manifest: &str) {
    stored_row(conn, url, raw_name, manifest);
    storage_set(conn, url, "k", "v").unwrap();
}

/// Older builds stored "unknown" for every manifest that named nothing, and an empty name is no
/// name either: two such rows are two different plugins, and neither may be deleted with its data.
#[test]
fn two_plugins_sharing_a_stand_in_name_are_both_kept() {
    let cases = [
        ("unknown", r#"{"main":"x.mjs"}"#, "{}"),
        ("", r#"{"name":""}"#, r#"{"name":""}"#),
    ];
    for (stored, manifest_a, manifest_b) in cases {
        let mut conn = mem();
        seeded(&mut conn, "https://a/x", stored, manifest_a);
        seeded(&mut conn, "https://b/y", stored, manifest_b);
        assert!(dedup_same_name(&mut conn).is_empty(), "{stored:?}");
        assert!(kept(&mut conn, "https://a/x"), "{stored:?}");
        assert!(kept(&mut conn, "https://b/y"), "{stored:?}");
    }
}

/// Two rows whose manifests both declare one name cannot both load: they would share its module
/// prefix and its data dir. The enabled one stays, whatever the name, "unknown" included.
#[test]
fn a_name_two_manifests_declare_is_collapsed_to_the_enabled_one() {
    for declared in ["Same", "unknown"] {
        let mut conn = mem();
        let manifest = format!(r#"{{"name":"{declared}"}}"#);
        seeded(&mut conn, "https://a/p", declared, &manifest);
        seeded(&mut conn, "https://b/p", declared, &manifest);
        disable(&mut conn, "https://b/p").unwrap();
        assert_eq!(
            dedup_same_name(&mut conn),
            vec![("https://b/p".to_string(), declared.to_string())],
            "{declared}"
        );
        assert!(kept(&mut conn, "https://a/p"), "{declared}");
        assert!(
            !list(&mut conn)
                .unwrap()
                .iter()
                .any(|p| p.url == "https://b/p"),
            "{declared}"
        );
        assert_eq!(
            storage_get(&mut conn, "https://b/p", "k"),
            None,
            "{declared}"
        );
    }
}

/// `enabled` plays no part: a plugin just disabled still writes from its `onUnload`.
#[test]
fn an_installed_plugin_writes_whether_or_not_it_is_enabled() {
    let mut conn = mem();
    let url = "https://a/Foo";
    install(&conn, url, &name("Foo"), "{}", "c", "h").unwrap();
    storage_set(&mut conn, url, "k", "v").expect("installed, not enabled");
    enable(&mut conn, url).unwrap();
    storage_set(&mut conn, url, "k", "v2").expect("installed, enabled");
    disable(&mut conn, url).unwrap();
    storage_set(&mut conn, url, "k", "v3").expect("installed, disabled again");
    assert_eq!(storage_get(&mut conn, url, "k").as_deref(), Some("v3"));
}

/// A read that fails is not a disable: the restart that asks would unload a running plugin.
#[test]
fn a_failed_read_of_the_enabled_flag_is_an_error() {
    let mut conn = mem();
    assert!(
        !is_enabled(&mut conn, "https://a/Foo").unwrap(),
        "an absent row is off"
    );
    let mut bare = Connection::open_in_memory().unwrap();
    assert!(
        is_enabled(&mut bare, "https://a/Foo").is_err(),
        "a failed read was folded"
    );
}

/// A new manifest alone is an update (its dependencies would go stale unseen); a source that
/// renames the plugin would take another name's data dir and trust rows, and changes nothing.
#[test]
fn an_update_takes_a_new_manifest_but_not_a_new_name() {
    let url = "https://a/Foo";
    let new_manifest = r#"{"name":"Foo","v":2}"#;
    let cases = [
        ("Foo", new_manifest, UpdateOutcome::Updated, new_manifest),
        (
            "Other",
            new_manifest,
            UpdateOutcome::Renamed {
                stored: "Foo".into(),
            },
            "{}",
        ),
    ];
    for (source_name, manifest, expected, kept) in cases {
        let mut conn = mem();
        stored_row(&mut conn, url, "Foo", "{}");
        let outcome = update(&mut conn, url, source_name, manifest, "code", "h2").unwrap();
        assert_eq!(outcome, expected, "{source_name}");
        assert_eq!(
            stored(&mut conn, url).unwrap().unwrap().manifest,
            kept,
            "{source_name}"
        );
    }
}

fn storage_rows(conn: &Connection, url: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM plugin_storage WHERE namespace = ?1",
        params![url],
        |row| row.get(0),
    )
    .unwrap()
}

/// A write let through before the uninstall and run after it. The write checks the row in its own
/// statement and finds no plugin to write for.
#[test]
fn a_storage_write_after_uninstall_leaves_no_row() {
    let mut conn = mem();
    let url = "https://a/Foo";
    install(&conn, url, &name("Foo"), "{}", "c", "h").unwrap();
    storage_set(&mut conn, url, "k", "v").unwrap();
    uninstall(&mut conn, url).unwrap();
    assert_eq!(storage_rows(&conn, url), 0);

    let late = storage_set(&mut conn, url, "late", "v");
    assert!(
        late.is_err(),
        "a write for an uninstalled plugin was accepted"
    );
    assert_eq!(storage_rows(&conn, url), 0, "orphan plugin_storage row");
}

/// A store move carries the enabled plugins under the old base, whatever the case of their url,
/// with their storage. A disabled one, another store's plugin and its storage stay.
#[test]
fn a_store_move_carries_enabled_plugins_and_their_storage() {
    let mut conn = mem();
    let (foo, bar, baz) = (
        "https://Old.example/store/Foo",
        "https://old.example/store/Bar",
        "https://other.example/store/Baz",
    );
    stored_row(&mut conn, foo, "Foo", "{}");
    stored_row(&mut conn, bar, "Bar", "{}");
    stored_row(&mut conn, baz, "Baz", "{}");
    disable(&mut conn, bar).unwrap();
    storage_set(&mut conn, foo, "k", "foo").unwrap();
    storage_set(&mut conn, baz, "k", "baz").unwrap();

    let moves =
        pending_store_move(&conn, "https://old.example/store/", "https://new.example").unwrap();
    let moved = "https://new.example/Foo";
    assert_eq!(
        moves,
        [StoreMove {
            name: "Foo".to_string(),
            from: foo.to_string(),
            to: moved.to_string(),
        }]
    );
    apply_store_move(&conn, &moves).unwrap();

    assert!(
        is_enabled(&mut conn, moved).unwrap(),
        "the moved plugin lost its flag"
    );
    assert_eq!(storage_get(&mut conn, moved, "k").as_deref(), Some("foo"));
    assert_eq!(
        storage_rows(&conn, foo),
        0,
        "storage left under the old url"
    );
    assert!(
        !is_enabled(&mut conn, bar).unwrap(),
        "the disabled plugin moved"
    );
    assert_eq!(storage_get(&mut conn, baz, "k").as_deref(), Some("baz"));
    assert!(is_enabled(&mut conn, baz).unwrap());
}

// What a plugin's switch-off or removal waits for.

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
    assert!(collect_enabled_code(&mut conn).unwrap().is_empty());
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
