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

// --- Luna metadata types (library plugin support) ---

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub(crate) struct LunaMeta {
    #[serde(rename = "type")]
    pub plugin_type: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<LunaDependency>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub(crate) struct LunaDependency {
    pub name: String,
    #[serde(rename = "storeUrl")]
    pub store_url: String,
    #[serde(rename = "devStoreUrl")]
    pub dev_store_url: Option<String>,
}

/// Parse the `luna` field from a plugin manifest JSON string.
/// - `Ok(None)`: no `luna` field - classic plugin without metadata
/// - `Ok(Some(meta))`: valid `luna` field
/// - `Err(msg)`: `luna` field present but malformed - caller must block the operation
pub(crate) fn parse_luna_meta(manifest: &str) -> Result<Option<LunaMeta>, String> {
    let json: serde_json::Value =
        serde_json::from_str(manifest).map_err(|e| format!("Invalid manifest JSON: {e}"))?;
    match json.get("luna") {
        None => Ok(None),
        Some(luna_value) => serde_json::from_value(luna_value.clone())
            .map(Some)
            .map_err(|e| format!("Invalid luna field in manifest: {e}")),
    }
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

    // Migration: ever_dispatched column for zombie cleanup guard
    match conn.execute(
        "ALTER TABLE plugins ADD COLUMN ever_dispatched INTEGER DEFAULT 0",
        [],
    ) {
        Ok(_) => {
            // New column - backfill ALL installed plugins (fail-closed:
            // any existing plugin may have been dispatched in a past session)
            conn.execute(
                "UPDATE plugins SET ever_dispatched = 1 WHERE installed = 1",
                [],
            )?;
        }
        Err(e) if e.to_string().contains("duplicate column name") => {
            // Already migrated
        }
        Err(e) => return Err(e),
    }

    Ok(())
}

/// Mark a plugin as having had its code dispatched at least once.
/// Called AFTER eval_js returns true. Separate from enable() (which is DB-first, before dispatch).
pub(crate) fn mark_ever_dispatched(conn: &mut Connection, url: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE plugins SET ever_dispatched = 1 WHERE url = ?1",
        params![url],
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

/// Store a plugin in the DB as installed but NOT enabled.
/// Activation happens separately via the `plugin.enable` IPC handler.
/// Returns an error if another installed plugin already has this name at a different URL.
pub(crate) fn install(
    conn: &mut Connection,
    url: &str,
    name: &str,
    manifest: &str,
    code: &str,
    hash: &str,
) -> Result<(), String> {
    if is_name_taken(conn, name, url) {
        return Err(format!(
            "Plugin '{name}' already installed at a different URL. Uninstall it first."
        ));
    }
    conn.execute(
        "INSERT OR REPLACE INTO plugins (url, name, manifest, code, hash, enabled, installed)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, 1)",
        params![url, name, manifest, code, hash],
    )
    .map_err(|e| format!("DB error: {e}"))?;
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

// --- Dependency graph helpers ---

/// Return names of installed plugins that depend on `plugin_name`.
/// Fail-closed: if any installed manifest has an invalid `luna` field, returns Err
/// (we cannot guarantee the plugin isn't a dependant → block the operation).
pub(crate) fn find_dependants(
    conn: &mut Connection,
    plugin_name: &str,
) -> Result<Vec<String>, String> {
    find_dependants_inner(conn, plugin_name, "installed = 1")
}

/// Return names of installed AND enabled plugins that depend on `plugin_name`.
/// Fail-closed on invalid manifests.
pub(crate) fn find_enabled_dependants(
    conn: &mut Connection,
    plugin_name: &str,
) -> Result<Vec<String>, String> {
    find_dependants_inner(conn, plugin_name, "installed = 1 AND enabled = 1")
}

fn find_dependants_inner(
    conn: &mut Connection,
    plugin_name: &str,
    where_clause: &str,
) -> Result<Vec<String>, String> {
    let sql = format!("SELECT name, manifest FROM plugins WHERE {where_clause}");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("Failed to query plugins: {e}"))?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|e| format!("Failed to query plugins: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    let mut dependants = Vec::new();
    for (name, manifest) in &rows {
        match parse_luna_meta(manifest) {
            Ok(None) => {} // no luna field - not a dependant
            Ok(Some(meta)) => {
                if meta.dependencies.iter().any(|d| d.name == plugin_name) {
                    dependants.push(name.clone());
                }
            }
            Err(msg) => {
                return Err(format!(
                    "Cannot verify dependants: plugin '{name}' has {msg}"
                ));
            }
        }
    }
    Ok(dependants)
}

/// Check that all dependencies of a plugin are installed AND enabled.
/// - `Ok(())`: all deps satisfied (or no deps)
/// - `Err(names)`: missing/disabled dep names, or error message if manifest is invalid
pub(crate) fn check_dependencies_satisfied(
    conn: &mut Connection,
    manifest: &str,
) -> Result<(), Vec<String>> {
    let meta = match parse_luna_meta(manifest) {
        Ok(None) => return Ok(()),
        Ok(Some(m)) => m,
        Err(msg) => return Err(vec![msg]),
    };
    if meta.dependencies.is_empty() {
        return Ok(());
    }

    let mut missing = Vec::new();
    for dep in &meta.dependencies {
        let satisfied: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM plugins WHERE name = ?1 AND installed = 1 AND enabled = 1",
                params![dep.name],
                |row| row.get::<_, i32>(0),
            )
            .unwrap_or(0)
            > 0;
        if !satisfied {
            missing.push(dep.name.clone());
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

/// Get the plugin name for a given URL.
pub(crate) fn get_name_by_url(conn: &mut Connection, url: &str) -> Option<String> {
    conn.query_row(
        "SELECT name FROM plugins WHERE url = ?1",
        params![url],
        |row| row.get(0),
    )
    .ok()
}

/// Check if another installed plugin (different URL) already has this name.
pub(crate) fn is_name_taken(conn: &mut Connection, name: &str, exclude_url: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM plugins WHERE name = ?1 AND url != ?2 AND installed = 1",
        params![name, exclude_url],
        |row| row.get::<_, i32>(0),
    )
    .unwrap_or(0)
        > 0
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

pub(crate) struct EnabledPlugin {
    pub url: String,
    pub name: String,
    pub code: String,
    pub manifest: String,
}

pub(crate) fn collect_enabled_code(conn: &mut Connection) -> Vec<EnabledPlugin> {
    let mut stmt = match conn.prepare(
        "SELECT url, name, code, manifest FROM plugins WHERE installed = 1 AND enabled = 1 AND code IS NOT NULL ORDER BY rowid",
    ) {
        Ok(s) => s,
        Err(e) => {
            crate::vprintln!("[PLUGINS] Failed to query enabled plugins: {e}");
            return Vec::new();
        }
    };
    match stmt.query_map([], |row| {
        Ok(EnabledPlugin {
            url: row.get(0)?,
            name: row.get(1)?,
            code: row.get(2)?,
            manifest: row.get(3)?,
        })
    }) {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(e) => {
            crate::vprintln!("[PLUGINS] Failed to query enabled plugins: {e}");
            Vec::new()
        }
    }
}

/// Deduplicate same-name plugins at startup.
/// Winner policy: prefer enabled=1 over enabled=0, then highest rowid (most recent).
/// Losers are DELETEd from both `plugins` and `plugin_storage`.
/// Returns `(url, name)` pairs of deleted duplicates.
pub(crate) fn dedup_same_name(conn: &mut Connection) -> Vec<(String, String)> {
    // Collect (name, url) pairs for losers - must finish all borrows before mutating
    let losers: Vec<(String, String)> = {
        let mut dup_stmt = match conn.prepare(
            "SELECT name FROM plugins WHERE installed = 1 GROUP BY name HAVING COUNT(*) > 1",
        ) {
            Ok(s) => s,
            Err(e) => {
                crate::vprintln!("[PLUGINS] dedup_same_name query failed: {e}");
                return Vec::new();
            }
        };
        let dup_names: Vec<String> = dup_stmt
            .query_map([], |row| row.get(0))
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();
        drop(dup_stmt);

        let mut result = Vec::new();
        for dup_name in &dup_names {
            let mut rows_stmt = match conn.prepare(
                "SELECT url FROM plugins WHERE installed = 1 AND name = ?1 ORDER BY enabled DESC, rowid DESC",
            ) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let urls: Vec<String> = rows_stmt
                .query_map(params![dup_name], |row| row.get(0))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default();
            // Skip the winner (first), collect losers
            for loser_url in urls.into_iter().skip(1) {
                result.push((dup_name.clone(), loser_url));
            }
        }
        result
    };

    // Now mutate: delete losers
    let mut removed = Vec::new();
    for (name, url) in losers {
        if let Err(e) = uninstall(conn, &url) {
            crate::vprintln!("[PLUGINS] dedup: failed to remove '{name}' ({url}): {e}");
        } else {
            removed.push((url, name));
        }
    }
    removed
}
