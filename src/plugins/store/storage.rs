use rusqlite::{Connection, params};

pub(crate) fn storage_get(conn: &mut Connection, namespace: &str, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM plugin_storage WHERE namespace = ?1 AND key = ?2",
        params![namespace, key],
        |row| row.get(0),
    )
    .ok()
}

/// Per-namespace storage quota limits. Passed as a parameter for the quota
/// logic to be unit-tested against tiny limits without large allocations.
struct StorageLimits {
    max_value_bytes: usize,
    max_key_bytes: usize,
    max_namespace_keys: i64,
    max_namespace_bytes: i64,
}

/// Production quota for `plugin.storage`. Generous for plugin settings; bounds a
/// single plugin's unbounded growth of its own namespace.
const STORAGE_LIMITS: StorageLimits = StorageLimits {
    max_value_bytes: 1024 * 1024,
    max_key_bytes: 1024,
    max_namespace_keys: 1000,
    max_namespace_bytes: 5 * 1024 * 1024,
};

/// Quota check before an INSERT OR REPLACE. Replace adds no key and shifts the
/// byte total by the delta; counts use `CAST(value AS BLOB)` for bytes not chars.
fn enforce_storage_quota(
    conn: &mut Connection,
    namespace: &str,
    key: &str,
    value: &str,
    limits: &StorageLimits,
) -> Result<(), String> {
    if value.len() > limits.max_value_bytes {
        return Err(format!(
            "value exceeds the {}-byte per-value limit",
            limits.max_value_bytes
        ));
    }
    if key.len() > limits.max_key_bytes {
        return Err(format!(
            "key exceeds the {}-byte per-key limit",
            limits.max_key_bytes
        ));
    }

    let (key_count, total_bytes): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(CAST(value AS BLOB))), 0)
             FROM plugin_storage WHERE namespace = ?1",
            params![namespace],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| format!("DB error: {e}"))?;

    // Existing byte length of this key (None if the key is new).
    let old_value_bytes: Option<i64> = conn
        .query_row(
            "SELECT LENGTH(CAST(value AS BLOB)) FROM plugin_storage WHERE namespace = ?1 AND key = ?2",
            params![namespace, key],
            |row| row.get(0),
        )
        .ok();

    if old_value_bytes.is_none() && key_count + 1 > limits.max_namespace_keys {
        return Err(format!(
            "namespace exceeds the {}-key limit",
            limits.max_namespace_keys
        ));
    }

    let new_total = total_bytes - old_value_bytes.unwrap_or(0) + value.len() as i64;
    if new_total > limits.max_namespace_bytes {
        return Err(format!(
            "namespace exceeds the {}-byte limit",
            limits.max_namespace_bytes
        ));
    }
    Ok(())
}

pub(crate) fn storage_set(
    conn: &mut Connection,
    namespace: &str,
    key: &str,
    value: &str,
) -> Result<(), String> {
    enforce_storage_quota(conn, namespace, key, value, &STORAGE_LIMITS)?;
    // The statement that writes checks the owner: a write let through before an uninstall and run
    // after it must not recreate rows under a url that no longer has a plugin.
    let written = conn
        .execute(
            "INSERT OR REPLACE INTO plugin_storage (namespace, key, value)
             SELECT ?1, ?2, ?3
             WHERE EXISTS (SELECT 1 FROM plugins WHERE url = ?1 AND installed = 1)",
            params![namespace, key, value],
        )
        .map_err(|e| format!("DB error: {e}"))?;
    if written == 0 {
        return Err("plugin is not installed".to_string());
    }
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

#[cfg(test)]
#[path = "../../../tests/unit/plugins/store/storage.rs"]
mod tests;
