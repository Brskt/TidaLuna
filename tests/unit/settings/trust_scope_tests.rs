//! Tests for which rows a trust revocation in `src/native_runtime/trust.rs` reaches, attached to
//! `src/settings.rs` by `#[path]`.

use super::fixture::mem_conn;
use super::*;
use crate::native_runtime::trust::{clear_all_trust, clear_trust_by_plugin, save_trust};
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
    let mut conn = mem_conn();
    for module in modules {
        save_trust(&mut conn, "h", module, "fs", true).unwrap();
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

/// `plugin.uninstall_all` clears everything by its own statement, not by a name that happens to
/// match every row.
#[test]
fn clearing_all_trust_leaves_nothing() {
    let mut conn = seeded(&["a/x.native.ts", "B", "@s/p/x.native.ts"]);
    clear_all_trust(&mut conn).unwrap();
    assert!(modules(&mut conn).is_empty());
}
