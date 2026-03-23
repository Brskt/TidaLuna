use rusqlite::{Connection, params};

#[derive(Debug, serde::Serialize)]
pub(crate) struct PluginInfo {
    pub url: String,
    pub name: String,
    pub manifest: String,
    pub hash: Option<String>,
    pub enabled: bool,
    pub installed: bool,
}

pub(crate) fn init_schema(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS plugins (
            url       TEXT PRIMARY KEY,
            name      TEXT NOT NULL,
            manifest  TEXT NOT NULL,
            code      TEXT,
            hash      TEXT,
            enabled   INTEGER DEFAULT 0,
            installed INTEGER DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS plugin_storage (
            namespace TEXT NOT NULL,
            key       TEXT NOT NULL,
            value     TEXT NOT NULL,
            PRIMARY KEY (namespace, key)
        );",
    )?;

    Ok(())
}

pub(crate) fn list(conn: &mut Connection) -> Vec<PluginInfo> {
    let mut stmt = match conn.prepare(
        "SELECT url, name, manifest, hash, enabled, installed FROM plugins WHERE installed = 1",
    ) {
        Ok(s) => s,
        Err(e) => {
            crate::vprintln!("[PLUGINS] Failed to list: {e}");
            return Vec::new();
        }
    };
    match stmt.query_map([], |row| {
        Ok(PluginInfo {
            url: row.get(0)?,
            name: row.get(1)?,
            manifest: row.get(2)?,
            hash: row.get(3)?,
            enabled: row.get::<_, i32>(4)? != 0,
            installed: row.get::<_, i32>(5)? != 0,
        })
    }) {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(e) => {
            crate::vprintln!("[PLUGINS] Failed to list: {e}");
            Vec::new()
        }
    }
}

pub(crate) fn install(
    conn: &mut Connection,
    url: &str,
    name: &str,
    manifest: &str,
    code: &str,
    hash: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO plugins (url, name, manifest, code, hash, enabled, installed)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, 1)",
        params![url, name, manifest, code, hash],
    )?;
    Ok(())
}

pub(crate) fn uninstall(conn: &mut Connection, url: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM plugins WHERE url = ?1", params![url])?;
    conn.execute(
        "DELETE FROM plugin_storage WHERE namespace = ?1",
        params![url],
    )?;
    Ok(())
}

pub(crate) fn uninstall_all(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM plugins", [])?;
    conn.execute("DELETE FROM plugin_storage", [])?;
    Ok(())
}

pub(crate) fn enable(conn: &mut Connection, url: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE plugins SET enabled = 1 WHERE url = ?1",
        params![url],
    )?;
    Ok(())
}

pub(crate) fn disable(conn: &mut Connection, url: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE plugins SET enabled = 0 WHERE url = ?1",
        params![url],
    )?;
    Ok(())
}

pub(crate) fn get_code(conn: &mut Connection, url: &str) -> Option<String> {
    conn.query_row(
        "SELECT code FROM plugins WHERE url = ?1",
        params![url],
        |row| row.get(0),
    )
    .ok()
}

pub(crate) fn storage_get(conn: &mut Connection, namespace: &str, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM plugin_storage WHERE namespace = ?1 AND key = ?2",
        params![namespace, key],
        |row| row.get(0),
    )
    .ok()
}

pub(crate) fn storage_set(
    conn: &mut Connection,
    namespace: &str,
    key: &str,
    value: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO plugin_storage (namespace, key, value)
         VALUES (?1, ?2, ?3)",
        params![namespace, key, value],
    )?;
    Ok(())
}

pub(crate) fn storage_del(
    conn: &mut Connection,
    namespace: &str,
    key: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM plugin_storage WHERE namespace = ?1 AND key = ?2",
        params![namespace, key],
    )?;
    Ok(())
}

pub(crate) fn storage_keys(conn: &mut Connection, namespace: &str) -> Vec<String> {
    let mut stmt = match conn.prepare("SELECT key FROM plugin_storage WHERE namespace = ?1") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    match stmt.query_map(params![namespace], |row| row.get(0)) {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

pub(crate) fn collect_enabled_code(conn: &mut Connection) -> Vec<(String, String, String)> {
    let plugins = list(conn);
    let mut result = Vec::new();
    for info in &plugins {
        if !info.enabled {
            continue;
        }
        let Some(code) = get_code(conn, &info.url) else {
            crate::vprintln!("[PLUGIN] No code found for '{}'", info.url);
            continue;
        };
        result.push((info.url.clone(), info.name.clone(), code));
    }
    result
}
