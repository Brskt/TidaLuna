use crate::db::{ROUTINE_SYNC, Stable};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::str::FromStr;

/// One persisted setting, declared once: its row and what each way of lacking a usable row
/// reads as. A load and its save cannot disagree on a name, and a setting cannot be declared
/// without choosing both values.
pub(crate) struct Setting<T: 'static> {
    key: &'static str,
    /// Never written: what a fresh profile gets.
    absent: T,
    /// A row that exists but cannot be used, or one a store rebuild lost: the value whose
    /// consequence the user would least regret.
    unreadable: T,
}

impl<T: PartialEq> Setting<T> {
    /// Whether a missing row reads as something the user could have refused: `absent` is not
    /// what the setting reads as when it cannot be used.
    fn grants_when_absent(&self) -> bool {
        self.absent != self.unreadable
    }

    /// A setting that grants when absent is on the disk before this returns: a
    /// refusal that only reached the OS cache would read as consent after a power cut.
    fn save(&self, conn: &Connection, value: &str) {
        if self.grants_when_absent() {
            let _stable = Stable::begin(conn);
            set(conn, self.key, value);
        } else {
            set(conn, self.key, value);
        }
    }
}

const WINDOW_X: Setting<i32> = Setting {
    key: "window.x",
    absent: i32::MIN,
    unreadable: i32::MIN,
};
const WINDOW_Y: Setting<i32> = Setting {
    key: "window.y",
    absent: i32::MIN,
    unreadable: i32::MIN,
};
const WINDOW_WIDTH: Setting<u32> = Setting {
    key: "window.width",
    absent: 1280,
    unreadable: 1280,
};
const WINDOW_HEIGHT: Setting<u32> = Setting {
    key: "window.height",
    absent: 800,
    unreadable: 800,
};
const WINDOW_MAXIMIZED: Setting<bool> = Setting {
    key: "window.maximized",
    absent: false,
    unreadable: false,
};
const WINDOW_CLOSE_TO_TRAY: Setting<bool> = Setting {
    key: "window.close_to_tray",
    absent: false,
    unreadable: false,
};
const PLAYER_VOLUME: Setting<Option<f64>> = Setting {
    key: "player.volume",
    absent: None,
    unreadable: None,
};
const PLAYER_VOLUME_UNMUTE: Setting<Option<f64>> = Setting {
    key: "player.volume_unmute",
    absent: None,
    unreadable: None,
};
const PLAYER_VOLUME_SYNC: Setting<bool> = Setting {
    key: "player.volume_sync",
    absent: true,
    unreadable: true,
};
const PLAYER_ASIO: Setting<bool> = Setting {
    key: "player.asio",
    absent: false,
    unreadable: false,
};
const PLAYER_CROSSFADE_ENABLED: Setting<bool> = Setting {
    key: "player.crossfade_enabled",
    absent: false,
    unreadable: false,
};
const PLAYER_CROSSFADE_SECS: Setting<u8> = Setting {
    key: "player.crossfade_secs",
    absent: 0,
    unreadable: 0,
};
// On by default, and on a row it cannot read: this setting gates a release lookup (two GETs) that
// offers an update, never a download.
const UPDATER_AUTO_CHECK: Setting<bool> = Setting {
    key: "updater.auto_check",
    absent: true,
    unreadable: true,
};
const UPDATER_CHANNEL: Setting<String> = Setting {
    key: "updater.channel",
    absent: String::new(),
    unreadable: String::new(),
};
const UPDATER_SKIP_VERSION: Setting<Option<String>> = Setting {
    key: "updater.skip_version",
    absent: None,
    unreadable: None,
};
// On by default, off when unknown: a refusal the store lost must not read as consent to a LAN
// listener.
const CONNECT_RECEIVER_ALWAYS_ON: Setting<bool> = Setting {
    key: "connect.receiver_always_on",
    absent: true,
    unreadable: false,
};
const LOGGING_LEVEL: Setting<u8> = Setting {
    key: "logging.level",
    absent: 0,
    unreadable: 0,
};
const LOGGING_CONSOLE: Setting<bool> = Setting {
    key: "logging.console",
    absent: false,
    unreadable: false,
};
// Cisco's binary licence requires the user to be able to enable and disable the library
// (condition 2), not to opt in. On by default, as Firefox ships it: animated covers work
// without a trip to settings, and a user who turns it off keeps the row that says so. Off when
// unknown: a refusal the store lost must not read as consent to a third-party download.
const VIDEO_CODEC_ENABLED: Setting<bool> = Setting {
    key: "video.codec_enabled",
    absent: true,
    unreadable: false,
};

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
            x: WINDOW_X.absent,
            y: WINDOW_Y.absent,
            width: WINDOW_WIDTH.absent,
            height: WINDOW_HEIGHT.absent,
            maximized: WINDOW_MAXIMIZED.absent,
        }
    }
}

impl WindowState {
    /// Returns true if the position was explicitly saved (not the sentinel default).
    pub fn has_position(&self) -> bool {
        self.x != i32::MIN && self.y != i32::MIN
    }
}

/// A table of the store and the schema version that added it. A file stamped below `since` was
/// written before the table existed: its absence is no loss. Version 0 is the schema of every
/// build published before the stamp, and a file with no stamp reads 0.
struct Table {
    since: i64,
    create: &'static str,
}

const SETTINGS: Table = Table {
    since: 0,
    create: "CREATE TABLE IF NOT EXISTS settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    )",
};

const NATIVE_TRUST: Table = Table {
    since: 0,
    create: "CREATE TABLE IF NOT EXISTS native_trust (
        code_hash TEXT NOT NULL,
        plugin    TEXT NOT NULL,
        module    TEXT NOT NULL,
        granted   INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (code_hash, plugin, module)
    )",
};

const REPAIR_NOTICE: Table = Table {
    since: 1,
    create: "CREATE TABLE IF NOT EXISTS repair_notice (
        id     INTEGER PRIMARY KEY CHECK (id = 1),
        notice TEXT NOT NULL
    )",
};

/// Every table of the store: a new one is declared here, with the version it brings.
const TABLES: [&Table; 3] = [&SETTINGS, &NATIVE_TRUST, &REPAIR_NOTICE];

/// What `init_schema` stamps in `user_version`: the newest version any table was added in.
const SCHEMA_VERSION: i64 = {
    let mut newest = 0;
    let mut at = 0;
    while at < TABLES.len() {
        if TABLES[at].since > newest {
            newest = TABLES[at].since;
        }
        at += 1;
    }
    newest
};

fn schema_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

pub(crate) fn init_schema(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", ROUTINE_SYNC)?;

    // The stamp lands with the tables it vouches for, or not at all. The write lock is taken up
    // front when the stamp is owed: a snapshot that began as a read cannot be upgraded once another
    // writer has committed.
    let stamped = schema_version(conn)?;
    let owed = stamped < SCHEMA_VERSION;
    let tx = conn.transaction_with_behavior(if owed {
        TransactionBehavior::Immediate
    } else {
        TransactionBehavior::Deferred
    })?;
    for table in TABLES {
        tx.execute(table.create, [])?;
    }
    if owed {
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    tx.commit()
}

/// Why a row could not be used, kept whole for the one line that reports it.
#[derive(Debug)]
pub(crate) enum Unreadable {
    Db(rusqlite::Error),
    Unparseable(String),
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "{e}"),
            Self::Unparseable(raw) => write!(f, "unparseable value {raw:?}"),
        }
    }
}

/// What one row read found. The fallback is the caller's.
enum Read<T> {
    Stored(T),
    Absent,
    Unreadable(Unreadable),
}

/// The pure read: no fallback, no logging. A row that is missing and one that could not be read
/// are different answers.
fn read<T: FromStr>(conn: &Connection, key: &str) -> Read<T> {
    match conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()
    {
        Ok(None) => Read::Absent,
        Ok(Some(raw)) => match raw.parse() {
            Ok(value) => Read::Stored(value),
            Err(_) => Read::Unreadable(Unreadable::Unparseable(raw)),
        },
        Err(e) => Read::Unreadable(Unreadable::Db(e)),
    }
}

/// A batch of reads that reports every failure in one `verr!` when it drops: a store that fails
/// every read must not spend the whole log budget on one line per key.
struct Reader<'c> {
    conn: &'c Connection,
    failures: Vec<(&'static str, Unreadable)>,
}

impl<'c> Reader<'c> {
    fn new(conn: &'c Connection) -> Self {
        Self {
            conn,
            failures: Vec::new(),
        }
    }

    /// The value for a row that gave none: `why` is the failure, `None` for a row never written.
    fn lacking<T: Clone>(&mut self, setting: &Setting<T>, why: Option<Unreadable>) -> T {
        match why {
            Some(why) => {
                self.failures.push((setting.key, why));
                setting.unreadable.clone()
            }
            None => setting.absent.clone(),
        }
    }

    fn get<T: FromStr + Clone>(&mut self, setting: &Setting<T>) -> T {
        match read(self.conn, setting.key) {
            Read::Stored(value) => value,
            Read::Absent => self.lacking(setting, None),
            Read::Unreadable(why) => self.lacking(setting, Some(why)),
        }
    }

    fn get_opt<T: FromStr + Clone>(&mut self, setting: &Setting<Option<T>>) -> Option<T> {
        match read(self.conn, setting.key) {
            Read::Stored(value) => Some(value),
            Read::Absent => self.lacking(setting, None),
            Read::Unreadable(why) => self.lacking(setting, Some(why)),
        }
    }

    #[cfg(test)]
    fn into_failures(mut self) -> Vec<(&'static str, Unreadable)> {
        std::mem::take(&mut self.failures)
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        if self.failures.is_empty() {
            return;
        }
        let list: Vec<String> = self
            .failures
            .iter()
            .map(|(key, why)| format!("{key} ({why})"))
            .collect();
        crate::verr!(
            "[SETTINGS] {} unreadable, fallbacks applied: {}",
            list.len(),
            list.join(", ")
        );
    }
}

fn write(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
        params![key, value],
    )
    .map(drop)
}

fn set(conn: &Connection, key: &str, value: &str) {
    if let Err(e) = write(conn, key, value) {
        crate::verr!("[SETTINGS] Failed to write {key}: {e}");
    }
}

pub(crate) fn load_window_state(conn: &mut Connection) -> WindowState {
    let mut r = Reader::new(conn);
    WindowState {
        x: r.get(&WINDOW_X),
        y: r.get(&WINDOW_Y),
        width: r.get(&WINDOW_WIDTH),
        height: r.get(&WINDOW_HEIGHT),
        maximized: r.get(&WINDOW_MAXIMIZED),
    }
}

pub(crate) fn save_window_state(conn: &mut Connection, state: &WindowState) {
    let tx = match conn.unchecked_transaction() {
        Ok(tx) => tx,
        Err(e) => {
            crate::verr!("[SETTINGS] Failed to begin transaction: {e}");
            return;
        }
    };
    WINDOW_X.save(&tx, &state.x.to_string());
    WINDOW_Y.save(&tx, &state.y.to_string());
    WINDOW_WIDTH.save(&tx, &state.width.to_string());
    WINDOW_HEIGHT.save(&tx, &state.height.to_string());
    WINDOW_MAXIMIZED.save(&tx, &state.maximized.to_string());
    if let Err(e) = tx.commit() {
        crate::verr!("[SETTINGS] Failed to commit transaction: {e}");
    }
}

pub(crate) fn save_maximized(conn: &mut Connection, maximized: bool) {
    WINDOW_MAXIMIZED.save(conn, &maximized.to_string());
}

#[cfg_attr(not(target_os = "windows"), expect(dead_code))]
pub(crate) fn load_volume_sync(conn: &mut Connection) -> bool {
    Reader::new(conn).get(&PLAYER_VOLUME_SYNC)
}

#[cfg_attr(not(target_os = "windows"), expect(dead_code))]
pub(crate) fn save_volume_sync(conn: &mut Connection, enabled: bool) {
    PLAYER_VOLUME_SYNC.save(conn, &enabled.to_string());
}

#[cfg_attr(not(target_os = "windows"), expect(dead_code))]
pub(crate) fn save_asio(conn: &mut Connection, enabled: bool) {
    PLAYER_ASIO.save(conn, &enabled.to_string());
}

/// The level the output last played at and the one an unmute gives back, in percent, as
/// `Player` keeps them for the next launch. `None` until a level was first set: the player then
/// starts at full scale, and TIDAL's own saved level is restored when a page loads.
pub(crate) fn load_output_level(conn: &mut Connection) -> Option<(f64, f64)> {
    let mut r = Reader::new(conn);
    let volume = r.get_opt(&PLAYER_VOLUME);
    let unmute = r.get_opt(&PLAYER_VOLUME_UNMUTE);
    Some((volume?, unmute?))
}

pub(crate) fn save_output_level(conn: &mut Connection, volume: f64, unmute: f64) {
    let tx = match conn.unchecked_transaction() {
        Ok(tx) => tx,
        Err(e) => {
            crate::verr!("[SETTINGS] Failed to begin transaction: {e}");
            return;
        }
    };
    PLAYER_VOLUME.save(&tx, &volume.to_string());
    PLAYER_VOLUME_UNMUTE.save(&tx, &unmute.to_string());
    if let Err(e) = tx.commit() {
        crate::verr!("[SETTINGS] Failed to commit transaction: {e}");
    }
}

pub(crate) fn save_close_to_tray(conn: &mut Connection, enabled: bool) {
    WINDOW_CLOSE_TO_TRAY.save(conn, &enabled.to_string());
}

pub(crate) fn load_update_auto_check(conn: &mut Connection) -> bool {
    Reader::new(conn).get(&UPDATER_AUTO_CHECK)
}

pub(crate) fn save_update_auto_check(conn: &mut Connection, enabled: bool) {
    UPDATER_AUTO_CHECK.save(conn, &enabled.to_string());
}

/// Update channel: "stable" (published releases only) or "dev" (the CI's
/// per-push prereleases too). Any other stored value reads as "stable".
pub(crate) fn load_update_channel(conn: &mut Connection) -> String {
    update_channel(&mut Reader::new(conn))
}

fn update_channel(r: &mut Reader) -> String {
    let value = r.get(&UPDATER_CHANNEL);
    if value == "dev" {
        value
    } else {
        "stable".to_string()
    }
}

pub(crate) fn save_update_channel(conn: &mut Connection, channel: &str) {
    let channel = if channel == "dev" { "dev" } else { "stable" };
    UPDATER_CHANNEL.save(conn, channel);
}

// The boot settings are read in production through `load_boot_settings`'s one `Reader`; these
// single-setting loads are what the tests pin each setting with.
#[cfg(test)]
pub(crate) fn load_receiver_always_on(conn: &mut Connection) -> bool {
    Reader::new(conn).get(&CONNECT_RECEIVER_ALWAYS_ON)
}

pub(crate) fn save_receiver_always_on(conn: &mut Connection, enabled: bool) {
    CONNECT_RECEIVER_ALWAYS_ON.save(conn, &enabled.to_string());
}

#[cfg(test)]
pub(crate) fn load_log_level(conn: &mut Connection) -> u8 {
    log_level(&mut Reader::new(conn))
}

fn log_level(r: &mut Reader) -> u8 {
    r.get(&LOGGING_LEVEL).min(crate::logging::MAX_LOG_LEVEL)
}

pub(crate) fn save_log_level(conn: &mut Connection, level: u8) {
    LOGGING_LEVEL.save(conn, &level.min(crate::logging::MAX_LOG_LEVEL).to_string());
}

#[cfg(test)]
pub(crate) fn load_crossfade_enabled(conn: &mut Connection) -> bool {
    Reader::new(conn).get(&PLAYER_CROSSFADE_ENABLED)
}

pub(crate) fn save_crossfade_enabled(conn: &mut Connection, enabled: bool) {
    PLAYER_CROSSFADE_ENABLED.save(conn, &enabled.to_string());
}

#[cfg(test)]
pub(crate) fn load_crossfade_secs(conn: &mut Connection) -> u8 {
    crossfade_secs(&mut Reader::new(conn))
}

/// Crossfade overlap in seconds. Clamped on read as well as on write: a value
/// edited into the database by hand must not reach the audio path out of range.
fn crossfade_secs(r: &mut Reader) -> u8 {
    r.get(&PLAYER_CROSSFADE_SECS)
        .min(crate::player::crossfade::MAX_CROSSFADE_SECS)
}

pub(crate) fn save_crossfade_secs(conn: &mut Connection, secs: u8) {
    PLAYER_CROSSFADE_SECS.save(
        conn,
        &secs
            .min(crate::player::crossfade::MAX_CROSSFADE_SECS)
            .to_string(),
    );
}

#[cfg(test)]
pub(crate) fn load_console(conn: &mut Connection) -> bool {
    Reader::new(conn).get(&LOGGING_CONSOLE)
}

pub(crate) fn save_console(conn: &mut Connection, enabled: bool) {
    LOGGING_CONSOLE.save(conn, &enabled.to_string());
}

#[cfg(test)]
pub(crate) fn load_video_codec_enabled(conn: &mut Connection) -> bool {
    Reader::new(conn).get(&VIDEO_CODEC_ENABLED)
}

pub(crate) fn save_video_codec_enabled(conn: &mut Connection, enabled: bool) {
    VIDEO_CODEC_ENABLED.save(conn, &enabled.to_string());
}

/// Settings the window bootstrap needs, batched into one db read, loading
/// off the CEF UI thread in main() instead of blocking on_context_initialized.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BootSettings {
    pub(crate) close_to_tray: bool,
    pub(crate) auto_check: bool,
    // "dev" channel as a bool: BootSettings stays Copy.
    pub(crate) update_dev_channel: bool,
    pub(crate) receiver_always_on: bool,
    pub(crate) volume_sync: bool,
    pub(crate) asio: bool,
    pub(crate) window_maximized: bool,
    pub(crate) log_level: u8,
    pub(crate) console: bool,
    pub(crate) crossfade_enabled: bool,
    pub(crate) crossfade_secs: u8,
    pub(crate) video_codec_enabled: bool,
}

/// One `Reader` for the whole boot read: a failing store reports one line, not one per key.
pub(crate) fn load_boot_settings(conn: &mut Connection) -> BootSettings {
    let mut r = Reader::new(conn);
    BootSettings {
        close_to_tray: r.get(&WINDOW_CLOSE_TO_TRAY),
        auto_check: r.get(&UPDATER_AUTO_CHECK),
        update_dev_channel: update_channel(&mut r) == "dev",
        receiver_always_on: r.get(&CONNECT_RECEIVER_ALWAYS_ON),
        // volume_sync is a Windows-only feature; non-Windows keeps it off.
        #[cfg(target_os = "windows")]
        volume_sync: r.get(&PLAYER_VOLUME_SYNC),
        #[cfg(not(target_os = "windows"))]
        volume_sync: false,
        // asio is a Windows-only output backend; non-Windows keeps it off.
        #[cfg(target_os = "windows")]
        asio: r.get(&PLAYER_ASIO),
        #[cfg(not(target_os = "windows"))]
        asio: false,
        window_maximized: r.get(&WINDOW_MAXIMIZED),
        log_level: log_level(&mut r),
        console: r.get(&LOGGING_CONSOLE),
        crossfade_enabled: r.get(&PLAYER_CROSSFADE_ENABLED),
        crossfade_secs: crossfade_secs(&mut r),
        video_codec_enabled: r.get(&VIDEO_CODEC_ENABLED),
    }
}

pub(crate) fn load_update_skip_version(conn: &mut Connection) -> Option<String> {
    Reader::new(conn).get_opt(&UPDATER_SKIP_VERSION)
}

pub(crate) fn save_update_skip_version(conn: &mut Connection, version: &str) {
    UPDATER_SKIP_VERSION.save(conn, version);
}

#[cfg(test)]
#[path = "../tests/unit/settings/fixture.rs"]
mod fixture;

#[cfg(test)]
#[path = "../tests/unit/settings/logging_settings_tests.rs"]
mod logging_settings_tests;

#[cfg(test)]
#[path = "../tests/unit/settings/crossfade_settings_tests.rs"]
mod crossfade_settings_tests;

#[cfg(test)]
#[path = "../tests/unit/settings/video_codec_settings_tests.rs"]
mod video_codec_settings_tests;

#[cfg(test)]
#[path = "../tests/unit/settings/output_level_settings_tests.rs"]
mod output_level_settings_tests;

#[cfg(test)]
#[path = "../tests/unit/settings/durability_settings_tests.rs"]
pub(crate) mod durability_settings_tests;

#[cfg(test)]
#[path = "../tests/unit/settings/durability_trust_tests.rs"]
mod durability_trust_tests;

#[cfg(test)]
#[path = "../tests/unit/settings/store_damage_tests.rs"]
mod store_damage_tests;
