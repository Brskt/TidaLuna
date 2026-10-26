//! Tests for which rows a trust revocation in `src/native_runtime/trust.rs` reaches, attached to
//! `src/settings.rs` by `#[path]`.

use super::fixture::mem_conn;
use super::*;
use crate::native_runtime::trust::{
    clear_all_trust, clear_trust_by_plugin, load_trust, save_trust,
};
use crate::plugins::name::PluginName;

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
