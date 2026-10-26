use crate::db::Stable;
use crate::plugins::name::PluginName;
use rusqlite::{Connection, params};

pub(crate) struct TrustDecision {
    pub module: String,
    pub granted: bool,
}

/// A plugin's trust decisions for its current code. A grant holds for its `code_hash` only, against
/// code substitution; a module whose latest decision under any hash refused stays refused until a
/// reinstall. A failed read is an error, never "none": a refusal missed lets the grants through.
pub(crate) fn load_trust(
    conn: &mut Connection,
    plugin: &str,
    code_hash: &str,
) -> rusqlite::Result<Vec<TrustDecision>> {
    let mut decisions = query_decisions(
        conn,
        "SELECT module, granted FROM native_trust WHERE plugin = ?1 AND code_hash = ?2",
        params![plugin, code_hash],
    )?;
    let refusals = query_decisions(
        conn,
        "SELECT module, granted FROM native_trust
         WHERE granted = 0 AND rowid IN (
             SELECT max(rowid) FROM native_trust WHERE plugin = ?1 GROUP BY module)",
        params![plugin],
    )?;
    for refusal in refusals {
        match decisions.iter_mut().find(|d| d.module == refusal.module) {
            Some(decision) => decision.granted = false,
            None => decisions.push(refusal),
        }
    }
    Ok(decisions)
}

fn query_decisions(
    conn: &mut Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<Vec<TrustDecision>> {
    let mut stmt = conn.prepare(sql)?;
    stmt.query_map(params, |row| {
        Ok(TrustDecision {
            module: row.get(0)?,
            granted: row.get::<_, i32>(1)? != 0,
        })
    })?
    .collect()
}

/// One trust dialog's answer as one commit, on the disk before this returns. Bun honours grants
/// module by module: a row stored in part, or a refusal a power cut takes back, would leave
/// modules granted that the user refused, an older hash's grant included.
pub(crate) fn save_trust_answers(
    conn: &mut Connection,
    code_hash: &str,
    plugin: &str,
    answers: &[(&str, bool)],
) -> rusqlite::Result<()> {
    let _stable = Stable::begin(conn);
    let tx = conn.unchecked_transaction()?;
    for &(module, granted) in answers {
        save_trust(&tx, code_hash, plugin, module, granted)?;
    }
    tx.commit()
}

pub(crate) fn save_trust(
    conn: &Connection,
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
/// hand an older grant back after a power cut, over any refusal given since.
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
