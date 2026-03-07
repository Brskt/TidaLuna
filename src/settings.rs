use rusqlite::{Connection, params};
use std::path::Path;

pub struct Settings {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct WindowState {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            x: i32::MIN,
            y: i32::MIN,
            width: 1280,
            height: 800,
            maximized: false,
        }
    }
}

impl WindowState {
    /// Returns true if the position was explicitly saved (not the sentinel default).
    #[allow(dead_code)] // used when window state restore is wired up
    pub fn has_position(&self) -> bool {
        self.x != i32::MIN && self.y != i32::MIN
    }
}

impl Settings {
    pub fn open(data_dir: &Path) -> rusqlite::Result<Self> {
        std::fs::create_dir_all(data_dir).ok();
        let db_path = data_dir.join("settings.db");

        let conn = Connection::open(&db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;

        Ok(Self { conn })
    }

    fn get(&self, key: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .ok()
    }

    #[allow(dead_code)] // used by save_window_state
    fn set(&self, key: &str, value: &str) {
        if let Err(e) = self.conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
            params![key, value],
        ) {
            crate::vprintln!("[SETTINGS] Failed to write {key}: {e}");
        }
    }

    pub fn load_window_state(&self) -> WindowState {
        let def = WindowState::default();
        WindowState {
            x: self
                .get("window.x")
                .and_then(|v| v.parse().ok())
                .unwrap_or(def.x),
            y: self
                .get("window.y")
                .and_then(|v| v.parse().ok())
                .unwrap_or(def.y),
            width: self
                .get("window.width")
                .and_then(|v| v.parse().ok())
                .unwrap_or(def.width),
            height: self
                .get("window.height")
                .and_then(|v| v.parse().ok())
                .unwrap_or(def.height),
            maximized: self
                .get("window.maximized")
                .and_then(|v| v.parse().ok())
                .unwrap_or(def.maximized),
        }
    }

    #[allow(dead_code)] // used when window state save is wired up
    pub fn save_window_state(&self, state: &WindowState) {
        let tx = match self.conn.unchecked_transaction() {
            Ok(tx) => tx,
            Err(e) => {
                crate::vprintln!("[SETTINGS] Failed to begin transaction: {e}");
                return;
            }
        };
        self.set("window.x", &state.x.to_string());
        self.set("window.y", &state.y.to_string());
        self.set("window.width", &state.width.to_string());
        self.set("window.height", &state.height.to_string());
        self.set("window.maximized", &state.maximized.to_string());
        if let Err(e) = tx.commit() {
            crate::vprintln!("[SETTINGS] Failed to commit transaction: {e}");
        }
    }
}
