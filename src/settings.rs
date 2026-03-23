use rusqlite::{Connection, params};

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
    pub fn has_position(&self) -> bool {
        self.x != i32::MIN && self.y != i32::MIN
    }
}

pub(crate) fn init_schema(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )?;

    Ok(())
}

fn get(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .ok()
}

fn set(conn: &Connection, key: &str, value: &str) {
    if let Err(e) = conn.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
        params![key, value],
    ) {
        crate::vprintln!("[SETTINGS] Failed to write {key}: {e}");
    }
}

pub(crate) fn load_window_state(conn: &mut Connection) -> WindowState {
    let def = WindowState::default();
    WindowState {
        x: get(conn, "window.x")
            .and_then(|v| v.parse().ok())
            .unwrap_or(def.x),
        y: get(conn, "window.y")
            .and_then(|v| v.parse().ok())
            .unwrap_or(def.y),
        width: get(conn, "window.width")
            .and_then(|v| v.parse().ok())
            .unwrap_or(def.width),
        height: get(conn, "window.height")
            .and_then(|v| v.parse().ok())
            .unwrap_or(def.height),
        maximized: get(conn, "window.maximized")
            .and_then(|v| v.parse().ok())
            .unwrap_or(def.maximized),
    }
}

pub(crate) fn save_window_state(conn: &mut Connection, state: &WindowState) {
    let tx = match conn.unchecked_transaction() {
        Ok(tx) => tx,
        Err(e) => {
            crate::vprintln!("[SETTINGS] Failed to begin transaction: {e}");
            return;
        }
    };
    set(&tx, "window.x", &state.x.to_string());
    set(&tx, "window.y", &state.y.to_string());
    set(&tx, "window.width", &state.width.to_string());
    set(&tx, "window.height", &state.height.to_string());
    set(&tx, "window.maximized", &state.maximized.to_string());
    if let Err(e) = tx.commit() {
        crate::vprintln!("[SETTINGS] Failed to commit transaction: {e}");
    }
}

pub(crate) fn save_maximized(conn: &mut Connection, maximized: bool) {
    set(conn, "window.maximized", &maximized.to_string());
}
