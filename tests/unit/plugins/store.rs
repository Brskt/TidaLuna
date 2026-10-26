//! Tests for `src/plugins/store.rs`, attached to it by `#[path]`.

use super::*;
use crate::plugins::name::PluginName;

fn mem() -> Connection {
    let mut conn = Connection::open_in_memory().expect("open in-memory db");
    init_schema(&mut conn).expect("init schema");
    conn
}

const TINY: StorageLimits = StorageLimits {
    max_value_bytes: 8,
    max_key_bytes: 4,
    max_namespace_keys: 2,
    max_namespace_bytes: 10,
};

#[test]
fn quota_rejects_an_oversized_value_or_key() {
    for (key, value, limit) in [("k", "123456789", "per-value"), ("toolong", "v", "per-key")] {
        let mut conn = mem();
        let err = enforce_storage_quota(&mut conn, "ns", key, value, &TINY).unwrap_err();
        assert!(err.contains(limit), "key {key:?}: got: {err}");
    }
}

#[test]
fn quota_caps_namespace_key_count_but_allows_replace() {
    let mut conn = mem();
    stored_row(&mut conn, "ns", "Ns", "{}");
    storage_set(&mut conn, "ns", "a", "1").expect("seed a");
    storage_set(&mut conn, "ns", "b", "2").expect("seed b");
    // A new third key exceeds the 2-key limit...
    let err = enforce_storage_quota(&mut conn, "ns", "c", "3", &TINY).unwrap_err();
    assert!(err.contains("key limit"), "got: {err}");
    // ...but replacing an existing key adds no key; it is allowed.
    enforce_storage_quota(&mut conn, "ns", "a", "9", &TINY).expect("replace allowed");
}

#[test]
fn quota_caps_namespace_byte_total() {
    let mut conn = mem();
    stored_row(&mut conn, "ns", "Ns", "{}");
    storage_set(&mut conn, "ns", "a", "12345").expect("seed"); // 5 bytes, cap 10
    // A new key pushing the namespace total to 11 bytes is rejected...
    let err = enforce_storage_quota(&mut conn, "ns", "b", "123456", &TINY).unwrap_err();
    assert!(err.contains("byte limit"), "got: {err}");
    // ...exactly at the cap (5 + 5 = 10) is allowed.
    enforce_storage_quota(&mut conn, "ns", "b", "12345", &TINY).expect("at cap allowed");
}

fn name(raw: &str) -> PluginName {
    PluginName::parse(raw).unwrap()
}

/// A row as an older build could have written it, name unchecked.
fn stored_row(conn: &mut Connection, url: &str, raw_name: &str, manifest: &str) {
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
    let listed = list(&mut conn);
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
    list(conn).iter().any(|p| p.url == url) && storage_get(conn, url, "k").as_deref() == Some("v")
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
            !list(&mut conn).iter().any(|p| p.url == "https://b/p"),
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

/// A write let through before the uninstall and run after it. The DB actor serves its queue in
/// order; the delete comes first and the write finds no plugin to write for.
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
