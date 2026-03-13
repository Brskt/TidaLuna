use rusqlite::{Connection, params};
use std::path::Path;

pub(crate) struct PluginStore {
    conn: Connection,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct PluginInfo {
    pub url: String,
    pub name: String,
    pub manifest: String,
    pub hash: Option<String>,
    pub enabled: bool,
    pub installed: bool,
}

impl PluginStore {
    pub fn open(data_dir: &Path) -> rusqlite::Result<Self> {
        if let Err(e) = std::fs::create_dir_all(data_dir) {
            crate::vprintln!(
                "[PLUGINS] Failed to create data dir {}: {e}",
                data_dir.display()
            );
        }
        let db_path = data_dir.join("plugins.db");
        let conn = Connection::open(&db_path)?;
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

        Ok(Self { conn })
    }

    pub fn list(&self) -> Vec<PluginInfo> {
        let mut stmt = match self.conn.prepare(
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

    pub fn install(
        &self,
        url: &str,
        name: &str,
        manifest: &str,
        code: &str,
        hash: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO plugins (url, name, manifest, code, hash, enabled, installed)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, 1)",
            params![url, name, manifest, code, hash],
        )?;
        Ok(())
    }

    pub fn uninstall(&self, url: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM plugins WHERE url = ?1", params![url])?;
        self.conn.execute(
            "DELETE FROM plugin_storage WHERE namespace = ?1",
            params![url],
        )?;
        Ok(())
    }

    pub fn uninstall_all(&self) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM plugins", [])?;
        self.conn.execute("DELETE FROM plugin_storage", [])?;
        Ok(())
    }

    pub fn enable(&self, url: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE plugins SET enabled = 1 WHERE url = ?1",
            params![url],
        )?;
        Ok(())
    }

    pub fn disable(&self, url: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE plugins SET enabled = 0 WHERE url = ?1",
            params![url],
        )?;
        Ok(())
    }

    pub fn get_code(&self, url: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT code FROM plugins WHERE url = ?1",
                params![url],
                |row| row.get(0),
            )
            .ok()
    }

    pub fn storage_get(&self, namespace: &str, key: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT value FROM plugin_storage WHERE namespace = ?1 AND key = ?2",
                params![namespace, key],
                |row| row.get(0),
            )
            .ok()
    }

    pub fn storage_set(&self, namespace: &str, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO plugin_storage (namespace, key, value)
             VALUES (?1, ?2, ?3)",
            params![namespace, key, value],
        )?;
        Ok(())
    }

    pub fn storage_del(&self, namespace: &str, key: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM plugin_storage WHERE namespace = ?1 AND key = ?2",
            params![namespace, key],
        )?;
        Ok(())
    }

    pub fn storage_keys(&self, namespace: &str) -> Vec<String> {
        let mut stmt = match self
            .conn
            .prepare("SELECT key FROM plugin_storage WHERE namespace = ?1")
        {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        match stmt.query_map(params![namespace], |row| row.get(0)) {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }
}
