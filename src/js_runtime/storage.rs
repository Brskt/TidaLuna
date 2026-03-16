use std::cell::RefCell;
use std::path::Path;

use rquickjs::{Ctx, Function, Result as JsResult};
use rusqlite::{Connection, params};

// Thread-local SQLite connection for plugin storage.
// Separate from the main PluginStore connection to avoid Mutex deadlocks
// when JS code calls __storage_* from within ctx.with().
thread_local! {
    static STORAGE_CONN: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

pub fn init_storage(db_path: &Path) -> anyhow::Result<()> {
    let conn =
        Connection::open(db_path).map_err(|e| anyhow::anyhow!("Failed to open storage DB: {e}"))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    STORAGE_CONN.with(|c| *c.borrow_mut() = Some(conn));
    Ok(())
}

/// Install `__storage_*` globals used by the idb-keyval shim.
pub fn install_storage(ctx: &Ctx<'_>) -> JsResult<()> {
    let globals = ctx.globals();

    globals.set(
        "__storage_get",
        Function::new(
            ctx.clone(),
            |ns: String, key: String| -> rquickjs::Result<Option<String>> {
                STORAGE_CONN.with(|c| {
                    let borrow = c.borrow();
                    let conn = borrow.as_ref().ok_or_else(|| {
                        rquickjs::Error::new_from_js_message(
                            "storage",
                            "connection",
                            "Storage not initialized",
                        )
                    })?;
                    Ok(conn
                        .query_row(
                            "SELECT value FROM plugin_storage WHERE namespace = ?1 AND key = ?2",
                            params![ns, key],
                            |row| row.get(0),
                        )
                        .ok())
                })
            },
        )?
        .with_name("__storage_get")?,
    )?;

    globals.set(
        "__storage_set",
        Function::new(
            ctx.clone(),
            |ns: String, key: String, value: String| -> rquickjs::Result<()> {
                STORAGE_CONN.with(|c| {
                    let borrow = c.borrow();
                    let conn = borrow.as_ref().ok_or_else(|| {
                        rquickjs::Error::new_from_js_message(
                            "storage",
                            "connection",
                            "Storage not initialized",
                        )
                    })?;
                    conn.execute(
                        "INSERT OR REPLACE INTO plugin_storage (namespace, key, value) VALUES (?1, ?2, ?3)",
                        params![ns, key, value],
                    )
                    .map_err(|e| {
                        rquickjs::Error::new_from_js_message("storage", "set", &e.to_string())
                    })?;
                    Ok(())
                })
            },
        )?
        .with_name("__storage_set")?,
    )?;

    globals.set(
        "__storage_del",
        Function::new(
            ctx.clone(),
            |ns: String, key: String| -> rquickjs::Result<()> {
                STORAGE_CONN.with(|c| {
                    let borrow = c.borrow();
                    let conn = borrow.as_ref().ok_or_else(|| {
                        rquickjs::Error::new_from_js_message(
                            "storage",
                            "connection",
                            "Storage not initialized",
                        )
                    })?;
                    conn.execute(
                        "DELETE FROM plugin_storage WHERE namespace = ?1 AND key = ?2",
                        params![ns, key],
                    )
                    .map_err(|e| {
                        rquickjs::Error::new_from_js_message("storage", "del", &e.to_string())
                    })?;
                    Ok(())
                })
            },
        )?
        .with_name("__storage_del")?,
    )?;

    globals.set(
        "__storage_keys",
        Function::new(ctx.clone(), |ns: String| -> rquickjs::Result<String> {
            STORAGE_CONN.with(|c| {
                let borrow = c.borrow();
                let conn = borrow.as_ref().ok_or_else(|| {
                    rquickjs::Error::new_from_js_message(
                        "storage",
                        "connection",
                        "Storage not initialized",
                    )
                })?;
                let mut stmt = conn
                    .prepare("SELECT key FROM plugin_storage WHERE namespace = ?1")
                    .map_err(|e| {
                        rquickjs::Error::new_from_js_message("storage", "keys", &e.to_string())
                    })?;
                let keys: Vec<String> = stmt
                    .query_map(params![ns], |row| row.get(0))
                    .map_err(|e| {
                        rquickjs::Error::new_from_js_message("storage", "keys", &e.to_string())
                    })?
                    .filter_map(|r| r.ok())
                    .collect();
                serde_json::to_string(&keys).map_err(|e| {
                    rquickjs::Error::new_from_js_message("storage", "keys", &e.to_string())
                })
            })
        })?
        .with_name("__storage_keys")?,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::js_runtime;
    use tempfile::tempdir;

    fn setup_test_storage() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("plugins.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS plugin_storage (
                namespace TEXT NOT NULL,
                key       TEXT NOT NULL,
                value     TEXT NOT NULL,
                PRIMARY KEY (namespace, key)
            );",
        )
        .unwrap();
        drop(conn);
        init_storage(&db_path).unwrap();
        dir
    }

    #[test]
    fn test_storage_set_and_get() {
        let _dir = setup_test_storage();
        let (_rt, ctx) = js_runtime::init().unwrap();
        ctx.with(|ctx| {
            install_storage(&ctx).unwrap();
            let _: () = ctx
                .eval(r#"__storage_set("test_ns", "key1", '{"hello":"world"}')"#)
                .unwrap();
            let result: String = ctx.eval(r#"__storage_get("test_ns", "key1")"#).unwrap();
            assert_eq!(result, r#"{"hello":"world"}"#);
        });
    }

    #[test]
    fn test_storage_get_missing() {
        let _dir = setup_test_storage();
        let (_rt, ctx) = js_runtime::init().unwrap();
        ctx.with(|ctx| {
            install_storage(&ctx).unwrap();
            let result: rquickjs::Value = ctx
                .eval(r#"__storage_get("test_ns", "nonexistent")"#)
                .unwrap();
            assert!(result.is_null() || result.is_undefined());
        });
    }

    #[test]
    fn test_storage_del() {
        let _dir = setup_test_storage();
        let (_rt, ctx) = js_runtime::init().unwrap();
        ctx.with(|ctx| {
            install_storage(&ctx).unwrap();
            let _: () = ctx.eval(r#"__storage_set("ns", "k", "v")"#).unwrap();
            let _: () = ctx.eval(r#"__storage_del("ns", "k")"#).unwrap();
            let result: rquickjs::Value = ctx.eval(r#"__storage_get("ns", "k")"#).unwrap();
            assert!(result.is_null() || result.is_undefined());
        });
    }

    #[test]
    fn test_storage_keys() {
        let _dir = setup_test_storage();
        let (_rt, ctx) = js_runtime::init().unwrap();
        ctx.with(|ctx| {
            install_storage(&ctx).unwrap();
            let _: () = ctx.eval(r#"__storage_set("ns", "a", "1")"#).unwrap();
            let _: () = ctx.eval(r#"__storage_set("ns", "b", "2")"#).unwrap();
            let result: String = ctx.eval(r#"__storage_keys("ns")"#).unwrap();
            let keys: Vec<String> = serde_json::from_str(&result).unwrap();
            assert!(keys.contains(&"a".to_string()));
            assert!(keys.contains(&"b".to_string()));
            assert_eq!(keys.len(), 2);
        });
    }
}
