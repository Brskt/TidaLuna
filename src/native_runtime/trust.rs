use rusqlite::{Connection, params};

pub(crate) struct TrustDecision {
    pub module: String,
    pub granted: bool,
}

/// Load persisted trust decisions for a plugin, deduplicated by module.
/// Not filtered by hash - settings extraction can produce slightly
/// different code (and thus a different hash) for the same plugin.
/// When multiple rows exist for the same module (different hashes),
/// keeps only the most recently inserted decision (highest rowid).
pub(crate) fn load_trust(conn: &mut Connection, plugin: &str) -> Vec<TrustDecision> {
    let mut stmt = match conn.prepare(
        "SELECT module, granted FROM native_trust WHERE plugin = ?1 \
         AND rowid IN (SELECT MAX(rowid) FROM native_trust WHERE plugin = ?1 GROUP BY module)",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map(params![plugin], |row| {
        Ok(TrustDecision {
            module: row.get(0)?,
            granted: row.get::<_, i32>(1)? != 0,
        })
    })
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

pub(crate) fn save_trust(
    conn: &mut Connection,
    code_hash: &str,
    plugin: &str,
    module: &str,
    granted: bool,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO native_trust (code_hash, plugin, module, granted) VALUES (?1, ?2, ?3, ?4)",
        params![code_hash, plugin, module, granted as i32],
    )?;
    Ok(())
}

/// Clear ALL trust decisions for a plugin (any code hash).
/// Called on plugin uninstall so reinstalling re-triggers trust dialogs.
/// Uses LIKE prefix match because native module names are "{pluginName}/xxx.native.ts".
/// The trailing '/' prevents matching "foobar" when clearing "foo".
pub(crate) fn clear_trust_by_plugin(conn: &mut Connection, plugin: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM native_trust WHERE plugin LIKE ?1",
        params![format!("{plugin}/%")],
    )?;
    Ok(())
}
