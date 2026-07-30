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
        );
        CREATE TABLE IF NOT EXISTS native_trust (
            code_hash TEXT NOT NULL,
            plugin    TEXT NOT NULL,
            module    TEXT NOT NULL,
            granted   INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (code_hash, plugin, module)
        );",
    )?;

    Ok(())
}

fn set(conn: &Connection, key: &str, value: &str) {
    if let Err(e) = conn.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
        params![key, value],
    ) {
        crate::vprintln!("[SETTINGS] Failed to write {key}: {e}");
    }
}

/// Read a settings value parsed as `T`, falling back to `default` when the key is
/// absent or unparseable.
fn get_parsed<T: std::str::FromStr>(conn: &Connection, key: &str, default: T) -> T {
    conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|s| s.parse().ok())
    .unwrap_or(default)
}

fn get_bool(conn: &Connection, key: &str, default: bool) -> bool {
    get_parsed(conn, key, default)
}

fn get_u8(conn: &Connection, key: &str, default: u8) -> u8 {
    get_parsed(conn, key, default)
}

pub(crate) fn load_window_state(conn: &mut Connection) -> WindowState {
    let mut ws = WindowState::default();
    let mut stmt = match conn.prepare(
        "SELECT key, value FROM settings WHERE key IN ('window.x', 'window.y', 'window.width', 'window.height', 'window.maximized')",
    ) {
        Ok(s) => s,
        Err(e) => {
            crate::vprintln!("[SETTINGS] Failed to load window state: {e}");
            return ws;
        }
    };
    let rows = match stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) {
        Ok(r) => r,
        Err(e) => {
            crate::vprintln!("[SETTINGS] Failed to load window state: {e}");
            return ws;
        }
    };
    for row in rows.flatten() {
        let (key, value) = row;
        match key.as_str() {
            "window.x" => {
                if let Ok(v) = value.parse() {
                    ws.x = v;
                }
            }
            "window.y" => {
                if let Ok(v) = value.parse() {
                    ws.y = v;
                }
            }
            "window.width" => {
                if let Ok(v) = value.parse() {
                    ws.width = v;
                }
            }
            "window.height" => {
                if let Ok(v) = value.parse() {
                    ws.height = v;
                }
            }
            "window.maximized" => {
                if let Ok(v) = value.parse() {
                    ws.maximized = v;
                }
            }
            _ => {}
        }
    }
    ws
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

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn load_volume_sync(conn: &mut Connection) -> bool {
    get_bool(conn, "player.volume_sync", true)
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn save_volume_sync(conn: &mut Connection, enabled: bool) {
    set(conn, "player.volume_sync", &enabled.to_string());
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn load_asio(conn: &mut Connection) -> bool {
    get_bool(conn, "player.asio", false)
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn save_asio(conn: &mut Connection, enabled: bool) {
    set(conn, "player.asio", &enabled.to_string());
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn load_exclusive(conn: &mut Connection) -> bool {
    get_bool(conn, "player.exclusive", false)
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn save_exclusive(conn: &mut Connection, enabled: bool) {
    set(conn, "player.exclusive", &enabled.to_string());
}

pub(crate) fn load_close_to_tray(conn: &mut Connection) -> bool {
    get_bool(conn, "window.close_to_tray", false)
}

pub(crate) fn save_close_to_tray(conn: &mut Connection, enabled: bool) {
    set(conn, "window.close_to_tray", &enabled.to_string());
}

pub(crate) fn load_update_auto_check(conn: &mut Connection) -> bool {
    get_bool(conn, "updater.auto_check", true)
}

pub(crate) fn save_update_auto_check(conn: &mut Connection, enabled: bool) {
    set(conn, "updater.auto_check", &enabled.to_string());
}

/// Update channel: "stable" (published releases only) or "dev" (the CI's
/// per-push prereleases too). Any other stored value reads as "stable".
pub(crate) fn load_update_channel(conn: &mut Connection) -> String {
    let value = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'updater.channel'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default();
    if value == "dev" {
        value
    } else {
        "stable".to_string()
    }
}

pub(crate) fn save_update_channel(conn: &mut Connection, channel: &str) {
    let channel = if channel == "dev" { "dev" } else { "stable" };
    set(conn, "updater.channel", channel);
}

pub(crate) fn load_receiver_always_on(conn: &mut Connection) -> bool {
    get_bool(conn, "connect.receiver_always_on", true)
}

pub(crate) fn save_receiver_always_on(conn: &mut Connection, enabled: bool) {
    set(conn, "connect.receiver_always_on", &enabled.to_string());
}

pub(crate) fn load_log_level(conn: &mut Connection) -> u8 {
    get_u8(conn, "logging.level", 0).min(crate::logging::MAX_LOG_LEVEL)
}

pub(crate) fn save_log_level(conn: &mut Connection, level: u8) {
    set(
        conn,
        "logging.level",
        &level.min(crate::logging::MAX_LOG_LEVEL).to_string(),
    );
}

pub(crate) fn load_console(conn: &mut Connection) -> bool {
    get_bool(conn, "logging.console", false)
}

pub(crate) fn save_console(conn: &mut Connection, enabled: bool) {
    set(conn, "logging.console", &enabled.to_string());
}

/// Settings the window bootstrap needs, batched into one db read so they load
/// off the CEF UI thread in main() instead of blocking on_context_initialized.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BootSettings {
    pub(crate) close_to_tray: bool,
    pub(crate) auto_check: bool,
    // "dev" channel as a bool so BootSettings stays Copy.
    pub(crate) update_dev_channel: bool,
    pub(crate) receiver_always_on: bool,
    pub(crate) volume_sync: bool,
    pub(crate) asio: bool,
    pub(crate) exclusive: bool,
    pub(crate) window_maximized: bool,
    pub(crate) log_level: u8,
    pub(crate) console: bool,
}

pub(crate) fn load_boot_settings(conn: &mut Connection) -> BootSettings {
    BootSettings {
        close_to_tray: load_close_to_tray(conn),
        auto_check: load_update_auto_check(conn),
        update_dev_channel: load_update_channel(conn) == "dev",
        receiver_always_on: load_receiver_always_on(conn),
        // volume_sync is a Windows-only feature; non-Windows keeps it off.
        #[cfg(target_os = "windows")]
        volume_sync: load_volume_sync(conn),
        #[cfg(not(target_os = "windows"))]
        volume_sync: false,
        // asio is a Windows-only output backend; non-Windows keeps it off.
        #[cfg(target_os = "windows")]
        asio: load_asio(conn),
        #[cfg(not(target_os = "windows"))]
        asio: false,
        // exclusive WASAPI is Windows-only; non-Windows keeps it off.
        #[cfg(target_os = "windows")]
        exclusive: load_exclusive(conn),
        #[cfg(not(target_os = "windows"))]
        exclusive: false,
        window_maximized: load_window_state(conn).maximized,
        log_level: load_log_level(conn),
        console: load_console(conn),
    }
}

pub(crate) fn load_update_skip_version(conn: &mut Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM settings WHERE key = 'updater.skip_version'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

pub(crate) fn save_update_skip_version(conn: &mut Connection, version: &str) {
    set(conn, "updater.skip_version", version);
}

#[cfg(test)]
#[path = "../tests/unit/settings/logging_settings_tests.rs"]
mod logging_settings_tests;
