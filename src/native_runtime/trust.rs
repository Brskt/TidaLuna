use crate::db::Stable;
use crate::plugins::name::PluginName;
use rusqlite::{Connection, params};

pub(crate) struct TrustDecision {
    pub module: String,
    pub granted: bool,
}

/// Load persisted trust decisions for a plugin's current code.
///
/// Scoped to `code_hash`: a decision granted for one version of the native
/// code does not apply once the code changes under the same plugin name. A
/// hash mismatch yields no decisions; the user is re-prompted, and that is
/// what makes trust-on-first-use sound against code substitution.
pub(crate) fn load_trust(
    conn: &mut Connection,
    plugin: &str,
    code_hash: &str,
) -> Vec<TrustDecision> {
    let mut stmt = match conn
        .prepare("SELECT module, granted FROM native_trust WHERE plugin = ?1 AND code_hash = ?2")
    {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map(params![plugin, code_hash], |row| {
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
/// Called on plugin uninstall and reinstall: the next load asks its trust dialogs again.
/// A plugin's modules are its bare name (the entry) and "{name}/xxx.native.ts". Matched by exact
/// bytes rather than `LIKE`, whose `_` is a wildcard and which ignores case.
///
/// The delete is on the disk before this returns: a clear that only reached the OS cache would
/// hand an older grant back after a power cut, over any refusal given since. Grants and refusals
/// stay on the routine sync, since a row lost with the tail only asks again.
pub(crate) fn clear_trust_by_plugin(
    conn: &mut Connection,
    plugin: &PluginName,
) -> rusqlite::Result<()> {
    let _stable = Stable::begin(conn);
    conn.execute(
        "DELETE FROM native_trust
         WHERE plugin = ?1 OR substr(plugin, 1, length(?1) + 1) = ?1 || '/'",
        params![plugin.as_str()],
    )?;
    Ok(())
}

/// Every plugin's trust decisions, for `plugin.uninstall_all`. On the disk before this returns,
/// for the same reason as `clear_trust_by_plugin`.
pub(crate) fn clear_all_trust(conn: &mut Connection) -> rusqlite::Result<()> {
    let _stable = Stable::begin(conn);
    conn.execute("DELETE FROM native_trust", [])?;
    Ok(())
}
