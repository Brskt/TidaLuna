use rusqlite::{Connection, params};

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

/// Return names of installed plugins that depend on `plugin_name`.
/// Fail-closed: if any installed manifest has an invalid `luna` field, returns Err
/// (we cannot guarantee the plugin isn't a dependant -> block the operation).
pub(crate) fn find_dependants(
    conn: &mut Connection,
    plugin_name: &str,
) -> Result<Vec<String>, String> {
    find_dependants_inner(conn, plugin_name, false)
}

/// Return names of installed AND enabled plugins that depend on `plugin_name`.
/// Fail-closed on invalid manifests.
pub(crate) fn find_enabled_dependants(
    conn: &mut Connection,
    plugin_name: &str,
) -> Result<Vec<String>, String> {
    find_dependants_inner(conn, plugin_name, true)
}

fn find_dependants_inner(
    conn: &mut Connection,
    plugin_name: &str,
    enabled_only: bool,
) -> Result<Vec<String>, String> {
    let sql = if enabled_only {
        "SELECT name, manifest FROM plugins WHERE installed = 1 AND enabled = 1"
    } else {
        "SELECT name, manifest FROM plugins WHERE installed = 1"
    };
    let mut stmt = conn
        .prepare(sql)
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
