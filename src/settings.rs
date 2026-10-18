use crate::db::{ROUTINE_SYNC, Stable};
use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};
use std::path::{Path, PathBuf};
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

/// The text a value is stored as: what its save writes and `FromStr` reads back. `None` is a value
/// no row stands for; a setting whose `unreadable` is `None` cannot be written down after a loss.
pub(crate) trait Stored {
    fn row(&self) -> Option<String>;
}

macro_rules! stored_as_text {
    ($($t:ty),*) => {
        $(impl Stored for $t {
            fn row(&self) -> Option<String> {
                Some(self.to_string())
            }
        })*
    };
}
stored_as_text!(bool, i32, u32, u8, String);

impl<T: ToString> Stored for Option<T> {
    fn row(&self) -> Option<String> {
        self.as_ref().map(ToString::to_string)
    }
}

impl<T: PartialEq + Stored> Setting<T> {
    /// Whether a missing row reads as something the user could have refused: `absent` is not
    /// what the setting reads as when it cannot be used.
    fn grants_when_absent(&self) -> bool {
        self.absent != self.unreadable
    }

    /// The row a loss leaves for this setting: its `unreadable` value, when a missing row would
    /// read as `absent` instead.
    fn lost_row(&self) -> Option<(&'static str, String)> {
        if !self.grants_when_absent() {
            return None;
        }
        self.unreadable.row().map(|row| (self.key, row))
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

/// Declares every setting and, from the same list, what a loss writes down: a setting declared
/// outside it would read its `absent` value after a loss, whatever the user had chosen.
macro_rules! declare_settings {
    ($(
        $(#[$m:meta])*
        const $name:ident: $t:ty = Setting { key: $k:expr, absent: $a:expr, unreadable: $u:expr, };
    )*) => {
        $( $(#[$m])* const $name: $t = Setting { key: $k, absent: $a, unreadable: $u }; )*

        fn lost_rows() -> Vec<(&'static str, String)> {
            [$( $name.lost_row() ),*].into_iter().flatten().collect()
        }
    };
}

declare_settings! {
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
    name: &'static str,
    since: i64,
    create: &'static str,
}

const SETTINGS: Table = Table {
    name: "settings",
    since: 0,
    create: "CREATE TABLE IF NOT EXISTS settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    )",
};

const NATIVE_TRUST: Table = Table {
    name: "native_trust",
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
    name: "repair_notice",
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

/// What opening the store found. Each outcome is announced differently, hence named.
/// `turned_off` lists the settings a possible loss wrote down as their `unreadable` value.
#[derive(Debug, PartialEq)]
pub(crate) enum Opened {
    Healthy,
    /// Only an index disagreed with its table: rebuilt from the table. `trust` is what the table
    /// had already lost, which rebuilding an index cannot bring back.
    Reindexed {
        turned_off: Vec<&'static str>,
        trust: Trust,
    },
    /// Rebuilt from the settings rows still readable, a copy of the damaged file kept beside it.
    Rebuilt {
        turned_off: Vec<&'static str>,
        trust: Trust,
    },
    /// The rebuilt file could not be put in place: the damaged one is where it was, and this run
    /// keeps its settings in memory.
    Volatile {
        turned_off: Vec<&'static str>,
        trust: Trust,
    },
}

/// What a rebuild did to the `native_trust` decisions, which it never carries over. Read from the
/// damaged file before anything is rewritten; the banner states a loss only when there was one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Trust {
    /// No decision was lost: the table held none, or a repair kept it.
    Empty,
    /// The table held decisions: native plugins ask again.
    Dropped,
    /// The table could not be read: native plugins may ask again.
    Unknown,
}

/// Both trees, since either can lie alone: a stale empty table page answers no rows while its
/// index still lists them. A read that fails is `Unknown`, never `Empty`.
fn trust_at_open(conn: &Connection) -> Trust {
    let found = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM native_trust NOT INDEXED)
             OR EXISTS(SELECT 1 FROM native_trust INDEXED BY sqlite_autoindex_native_trust_1)",
        [],
        |row| row.get::<_, bool>(0),
    );
    match found {
        Ok(true) => Trust::Dropped,
        Ok(false) => Trust::Empty,
        Err(_) => Trust::Unknown,
    }
}

/// What `REINDEX` cannot give back. It rebuilds an index from its table; only a row the index
/// lists and the table no longer holds is gone: a row that merely exists would announce a loss on
/// every healthy table. Read before it runs, since afterwards the two agree on the loss. A read
/// that fails is `Unknown`, never `Empty`. `count(*)` is answered from the table whatever index is
/// named; `count(code_hash)` walks the index.
fn trust_lost_to_reindex(conn: &Connection) -> Trust {
    let counts = conn.query_row(
        "SELECT (SELECT count(code_hash) FROM native_trust NOT INDEXED),
                (SELECT count(code_hash) FROM native_trust INDEXED BY sqlite_autoindex_native_trust_1)",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    );
    match counts {
        Ok((table, index)) if index > table => Trust::Dropped,
        Ok(_) => Trust::Empty,
        Err(_) => Trust::Unknown,
    }
}

/// What the user is told about a repair, in the page's banner.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairNotice {
    turned_off: Vec<&'static str>,
    trust: Trust,
    /// Changes reach the disk again.
    saved: bool,
}

impl Opened {
    /// `None` when nothing the user would notice changed.
    pub(crate) fn notice(&self) -> Option<RepairNotice> {
        let (turned_off, trust, saved) = match self {
            Self::Healthy => return None,
            Self::Reindexed {
                turned_off,
                trust: Trust::Empty,
            } if turned_off.is_empty() => return None,
            Self::Reindexed { turned_off, trust } | Self::Rebuilt { turned_off, trust } => {
                (turned_off, *trust, true)
            }
            Self::Volatile { turned_off, trust } => (turned_off, *trust, false),
        };
        Some(RepairNotice {
            turned_off: turned_off.clone(),
            trust,
            saved,
        })
    }
}

/// What the `repair_notice` row holds. The keys come back as the declared names: one this build
/// does not declare cannot be told to the user.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredNotice {
    turned_off: Vec<String>,
    trust: Trust,
    saved: bool,
}

impl Trust {
    /// The wider of two losses: a decision known dropped over one that may be, over none.
    fn widest(self, other: Self) -> Self {
        match (self, other) {
            (Self::Dropped, _) | (_, Self::Dropped) => Self::Dropped,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            _ => Self::Empty,
        }
    }
}

/// The repair the store held a notice of when a new one is made.
enum Earlier {
    Nothing,
    Told(RepairNotice),
    /// A notice was there, or may have been, and what it said is gone.
    Unreadable,
}

/// The settings reading as a loss writes them down: what an earlier repair turned off, or what
/// the user chose to turn off, which a row cannot tell apart. A row that does not read counts, as
/// it reads as that value too.
fn marked_off(store: &Connection) -> Vec<&'static str> {
    lost_rows()
        .into_iter()
        .filter(|(key, row)| {
            match store
                .query_row(
                    "SELECT value FROM settings WHERE key = ?1",
                    params![key],
                    |found| found.get::<_, String>(0),
                )
                .optional()
            {
                Ok(found) => found.as_ref() == Some(row),
                Err(_) => true,
            }
        })
        .map(|(key, _)| key)
        .collect()
}

impl RepairNotice {
    /// This repair told together with the one the user has not yet been told of.
    fn after(mut self, earlier: &Earlier, store: &Connection) -> Self {
        match earlier {
            Earlier::Nothing => {}
            Earlier::Told(earlier) => {
                for key in &earlier.turned_off {
                    if !self.turned_off.contains(key) {
                        self.turned_off.push(key);
                    }
                }
                self.trust = self.trust.widest(earlier.trust);
            }
            Earlier::Unreadable => {
                for key in marked_off(store) {
                    if !self.turned_off.contains(&key) {
                        self.turned_off.push(key);
                    }
                }
                self.trust = self.trust.widest(Trust::Unknown);
            }
        }
        self
    }

    /// All that can still be told of a notice that cannot be read: every setting that reads as a
    /// repair leaves it, and permissions that may be asked again.
    fn unreadable_earlier(store: &Connection) -> Self {
        Self {
            turned_off: marked_off(store),
            trust: Trust::Unknown,
            saved: true,
        }
    }
}

/// The notice a repair is about to replace the store of. A read that fails here is a lost fact,
/// not a missing one: the repair renames a new file over the one that held it.
fn earlier_notice(conn: &Connection, table_vanished: bool) -> Earlier {
    if table_vanished {
        crate::verr!("[SETTINGS] the repair notice table left the schema, told as a wider loss");
        return Earlier::Unreadable;
    }
    match read_notice(conn) {
        Ok(Some(notice)) => Earlier::Told(notice),
        Ok(None) => Earlier::Nothing,
        Err(e) => {
            crate::verr!(
                "[SETTINGS] an earlier repair notice cannot be read, told as a wider loss: {e}"
            );
            Earlier::Unreadable
        }
    }
}

/// Writes what `opened` owes the user into the store it describes, with `earlier`, a repair never
/// told of, folded in. The row travels with the repair: inside the transaction that makes a
/// reindex real, inside the file a rebuild renames into place. It stays until the page confirms
/// the banner is on the screen.
fn record_notice(conn: &Connection, opened: &Opened, earlier: &Earlier) -> rusqlite::Result<()> {
    let notice = match (opened.notice(), earlier) {
        (Some(notice), earlier) => notice.after(earlier, conn),
        // This repair tells nothing, but the row an earlier one left is unreadable: replaced, not
        // left to fail again at every launch.
        (None, Earlier::Unreadable) => RepairNotice::unreadable_earlier(conn),
        (None, _) => return Ok(()),
    };
    let json = serde_json::to_string(&notice)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT OR REPLACE INTO repair_notice (id, notice) VALUES (1, ?1)",
        params![json],
    )
    .map(drop)
}

fn read_notice(conn: &Connection) -> rusqlite::Result<Option<RepairNotice>> {
    let Some(json) = conn
        .query_row("SELECT notice FROM repair_notice WHERE id = 1", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
    else {
        return Ok(None);
    };
    let stored: StoredNotice = serde_json::from_str(&json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let declared: Vec<&'static str> = lost_rows().into_iter().map(|(key, _)| key).collect();
    Ok(Some(RepairNotice {
        turned_off: declared
            .into_iter()
            .filter(|key| stored.turned_off.iter().any(|named| named == key))
            .collect(),
        trust: stored.trust,
        saved: stored.saved,
    }))
}

pub(crate) fn corruption(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    )
}

/// Whether `PRAGMA integrity_check` found nothing. A check that cannot get through a damaged
/// page answers with the corruption code instead of rows, which is "not clean" as well.
fn is_clean(conn: &Connection) -> rusqlite::Result<bool> {
    let rows = conn.prepare("PRAGMA integrity_check").and_then(|mut stmt| {
        stmt.query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
    });
    match rows {
        Ok(rows) => Ok(rows == ["ok"]),
        Err(e) if corruption(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

struct Salvaged {
    rows: Vec<(String, String)>,
    /// False when a row could not be read, or the scan did not reach the end of the table. The
    /// copy's own outcome decides the loss: a check of the file's structure passes a value whose
    /// type flipped to a blob.
    intact: bool,
}

/// The settings rows the table itself still yields, its damaged index left out. `native_trust`
/// is not carried: a single byte (serial type 8 to 9) turns a stored refusal into a grant.
/// What a failed check touched is asked again. A row that does not read is skipped rather than
/// ending the scan; the rows after it still carry over.
fn salvage(conn: &Connection) -> Salvaged {
    let mut salvaged = Salvaged {
        rows: Vec::new(),
        intact: false,
    };
    let Ok(mut stmt) = conn.prepare("SELECT key, value FROM settings NOT INDEXED") else {
        return salvaged;
    };
    let Ok(mut rows) = stmt.query([]) else {
        return salvaged;
    };
    let mut skipped = false;
    loop {
        match rows.next() {
            Ok(Some(row)) => match (row.get::<_, String>(0), row.get::<_, String>(1)) {
                (Ok(key), Ok(value)) => salvaged.rows.push((key, value)),
                _ => skipped = true,
            },
            Ok(None) => {
                salvaged.intact = !skipped;
                return salvaged;
            }
            Err(_) => return salvaged,
        }
    }
}

/// A file that already holds pages, was written since the table existed, and whose schema no
/// longer lists it: `init_schema` would put an empty table in its place, and an empty table passes
/// its own check. A file stamped before the table was added never had it.
fn table_vanished(conn: &Connection, table: &Table) -> rusqlite::Result<bool> {
    let pages: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let listed: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name = ?1",
        params![table.name],
        |row| row.get(0),
    )?;
    Ok(pages > 0 && listed == 0 && schema_version(conn)? >= table.since)
}

/// The tables `init_schema` would put back empty, hiding what they held from anything read after
/// it. Only `settings` stops the schema pass: the other two widen what a repair tells.
struct Vanished {
    settings: bool,
    trust: bool,
    notice: bool,
}

/// Whether the `settings` table and its index pass their own check. The full check cannot say:
/// it blames a page two trees claim on whichever it walks second, and names no tree at all for
/// some damage. A check that errors is not clean.
fn settings_clean(conn: &Connection) -> bool {
    let rows = conn
        .prepare("PRAGMA integrity_check('settings')")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        });
    matches!(rows, Ok(rows) if rows == ["ok"])
}

/// Writes down, after a possible loss, the `unreadable` value of every setting that has no row
/// and would otherwise read as `absent`: a refusal the store lost must not read as consent.
fn mark_lost(
    conn: &Connection,
    has_row: impl Fn(&str) -> bool,
) -> rusqlite::Result<Vec<&'static str>> {
    let mut turned_off = Vec::new();
    for (key, row) in lost_rows() {
        if !has_row(key) {
            write(conn, key, &row)?;
            turned_off.push(key);
        }
    }
    if !turned_off.is_empty() {
        crate::verr!(
            "[SETTINGS] settings.db may have lost rows, set to their safe value: {}",
            turned_off.join(", ")
        );
    }
    Ok(turned_off)
}

fn has_row(conn: &Connection, key: &str) -> bool {
    matches!(
        conn.query_row(
            "SELECT 1 FROM settings WHERE key = ?1",
            params![key],
            |_| { Ok(()) }
        )
        .optional(),
        Ok(Some(()))
    )
}

/// The new store, whole, in one transaction: the carried rows and the markers of what is lost
/// land together or not at all.
fn populate(
    conn: &mut Connection,
    carried: &[(String, String)],
    settings_lost: bool,
) -> rusqlite::Result<Vec<&'static str>> {
    init_schema(conn)?;
    let tx = conn.transaction()?;
    for (key, value) in carried {
        write(&tx, key, value)?;
    }
    let turned_off = if settings_lost {
        mark_lost(&tx, |key| carried.iter().any(|(carried, _)| carried == key))?
    } else {
        Vec::new()
    };
    tx.commit()?;
    Ok(turned_off)
}

/// Copies of damaged stores kept beside the live one; older ones are dropped first.
const KEPT_ASIDES: usize = 3;

/// A name no file holds, with its two companions free too: a later incident never lands on an
/// earlier one's file, nor on a `-wal` SQLite would replay into it.
pub(crate) fn aside_name(path: &Path) -> PathBuf {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    (0u32..)
        .map(|n| beside(path, &format!(".corrupt-{ms}-{n}")))
        .find(|aside| {
            ["", "-wal", "-shm"]
                .iter()
                .all(|suffix| !beside(aside, suffix).exists())
        })
        .expect("an unused name exists")
}

/// Drops all but the newest `KEPT_ASIDES` copies, each with its companions.
fn prune_asides(path: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return;
    };
    let prefix = format!("{}.corrupt-", name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut asides: Vec<(u128, u32, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let (ms, n) = file_name.strip_prefix(&prefix)?.split_once('-')?;
            Some((ms.parse().ok()?, n.parse().ok()?, entry.path()))
        })
        .collect();
    asides.sort();
    let surplus = asides.len().saturating_sub(KEPT_ASIDES);
    for (_, _, aside) in asides.into_iter().take(surplus) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(beside(&aside, suffix));
        }
    }
}

/// Copies `from` into a new file at `to`, in one SQLite transaction on the destination. On unix the
/// online backup opens the file by the bytes of its path, as `Connection::open` does; `VACUUM INTO`
/// takes its filename as SQL text, which a path that is not UTF-8 cannot be. The destination
/// connection is closed when this returns (Windows renames only a file no handle is open on).
fn write_whole(from: &Connection, to: &Path) -> Result<(), String> {
    let mut dest = Connection::open(to).map_err(|e| e.to_string())?;
    let backup = Backup::new(from, &mut dest).map_err(|e| e.to_string())?;
    match backup.step(-1) {
        Ok(StepResult::Done) => Ok(()),
        Ok(other) => Err(format!("backup stopped at {other:?}")),
        Err(e) => Err(e.to_string()),
    }
}

/// Puts the store held in `fresh` at `path`. It is written whole beside the damaged file first and
/// renamed over it last; at every instant `path` holds the old store or the complete new one:
/// the damaged file is copied aside, never moved. The caller has dropped its connection to `path`
/// (Windows renames only a file no handle is open on: SQLite opens without `FILE_SHARE_DELETE`).
pub(crate) fn replace(path: &Path, fresh: &Connection) -> Result<PathBuf, String> {
    let aside = aside_name(path);
    replace_naming(path, fresh, &aside)?;
    Ok(aside)
}

/// `replace` with the copy's name chosen by the caller, for a store that has to carry that name
/// inside itself. What `fresh` holds is renamed into place whole; a fact written into it before
/// the call is there exactly when the replacement is.
pub(crate) fn replace_naming(path: &Path, fresh: &Connection, aside: &Path) -> Result<(), String> {
    let new = beside(path, ".new");
    for suffix in ["", "-wal", "-shm", "-journal"] {
        match std::fs::remove_file(beside(&new, suffix)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!("clear {}: {e}", beside(&new, suffix).display()));
            }
            _ => {}
        }
    }
    if let Err(e) = write_whole(fresh, &new) {
        // A half-written file the next launch would only clear.
        let _ = std::fs::remove_file(&new);
        return Err(format!("write {}: {e}", new.display()));
    }
    // A `-wal` at the target would pair with the new file: it is only ever empty or absent once
    // the old connection is closed, unless another process still holds the store.
    if std::fs::metadata(beside(path, "-wal")).is_ok_and(|m| m.len() > 0) {
        let _ = std::fs::remove_file(&new);
        return Err("the damaged store still holds a -wal".into());
    }
    if let Err(e) = std::fs::copy(path, aside) {
        let _ = std::fs::remove_file(aside);
        let _ = std::fs::remove_file(&new);
        return Err(format!("copy aside: {e}"));
    }
    if let Err(e) = std::fs::rename(&new, path) {
        // The old store stays: its copy is a backup of nothing.
        let _ = std::fs::remove_file(aside);
        let _ = std::fs::remove_file(&new);
        return Err(format!("rename over: {e}"));
    }
    prune_asides(path);
    Ok(())
}

/// The new store is built in memory, where nothing can fail on a disk, then put in place. When
/// that fails, the damaged file is still where it was, and this run keeps the same store in memory.
fn rebuild(
    path: &Path,
    carried: &[(String, String)],
    settings_lost: bool,
    trust: Trust,
    earlier: Earlier,
) -> rusqlite::Result<(Connection, Opened)> {
    let mut fresh = Connection::open_in_memory()?;
    let turned_off = populate(&mut fresh, carried, settings_lost)?;
    // Inside the file the swap renames into place: the repair and the duty to tell of it are
    // one rename.
    let rebuilt = Opened::Rebuilt {
        turned_off: turned_off.clone(),
        trust,
    };
    record_notice(&fresh, &rebuilt, &earlier)?;
    let placed = replace(path, &fresh).and_then(|aside| {
        crate::verr!(
            "[SETTINGS] settings.db was damaged, a copy is kept as {}; {} settings carried over",
            aside.display(),
            carried.len()
        );
        Connection::open(path)
            .and_then(|mut conn| init_schema(&mut conn).map(|()| conn))
            .map_err(|e| format!("reopen: {e}"))
    });
    match placed {
        Ok(conn) => Ok((conn, Opened::Rebuilt { turned_off, trust })),
        Err(why) => {
            crate::verr!(
                "[SETTINGS] cannot rebuild settings.db ({why}), settings stay in memory this run"
            );
            // The row written for the file that was not placed says what this run cannot do.
            let volatile = Opened::Volatile { turned_off, trust };
            record_notice(&fresh, &volatile, &earlier)?;
            Ok((fresh, volatile))
        }
    }
}

/// `REINDEX` and the markers for what the table lost, in one transaction: a crash between the two
/// must not leave a repaired index with no trace of the loss. `None` when the index will not
/// rebuild, or the file is still not clean after it.
fn reindex(
    conn: &Connection,
    settings_lost: bool,
    earlier: &Earlier,
) -> rusqlite::Result<Option<Opened>> {
    let tx = conn.unchecked_transaction()?;
    let trust = trust_lost_to_reindex(&tx);
    if tx.execute_batch("REINDEX").is_err() || !is_clean(&tx).unwrap_or(false) {
        return Ok(None);
    }
    let turned_off = if settings_lost {
        mark_lost(&tx, |key| has_row(&tx, key))?
    } else {
        Vec::new()
    };
    let reindexed = Opened::Reindexed { turned_off, trust };
    record_notice(&tx, &reindexed, earlier)?;
    tx.commit()?;
    Ok(Some(reindexed))
}

/// Opens settings.db behind `PRAGMA integrity_check`. The check sees what a read cannot: an index
/// that lost a row answers `Ok(None)`, the same as a setting never written. Corruption
/// (`CORRUPT`, `NOTADB`) that `REINDEX` does not clear is rebuilt, a copy of the damaged file
/// kept beside it. Anything else (`BUSY`, `READONLY`, `IOERR`, `CANTOPEN`) returns with the file
/// untouched: a transient fault is no reason to discard what the user chose.
///
/// Settings rows count as possibly lost only when the `settings` table itself fails, has left the
/// schema, or did not hand every row to the salvage: damage confined to `native_trust` leaves
/// every setting as the user chose it.
pub(crate) fn open(path: &Path) -> rusqlite::Result<(Connection, Opened)> {
    let opened = Connection::open(path).and_then(|mut conn| {
        let vanished = Vanished {
            settings: table_vanished(&conn, &SETTINGS)?,
            trust: table_vanished(&conn, &NATIVE_TRUST)?,
            notice: table_vanished(&conn, &REPAIR_NOTICE)?,
        };
        // `init_schema` would put an empty `settings` in the place of the vanished one, and the
        // next launch would find nothing missing.
        if !vanished.settings {
            init_schema(&mut conn)?;
        }
        let clean = is_clean(&conn)?;
        Ok((conn, clean, vanished))
    });
    match opened {
        Ok((
            conn,
            true,
            Vanished {
                settings: false, ..
            },
        )) => Ok((conn, Opened::Healthy)),
        Ok((conn, _, vanished)) => {
            // Asked before REINDEX, which rebuilds an index from its table: a row the table lost
            // would leave the index too, and every check would pass.
            let settings_lost = !settings_clean(&conn);
            // Before REINDEX for the same reason: a row the trust table lost would leave its
            // index too. A table that left the schema was put back empty by `init_schema`: it
            // reads as holding nothing, which is not known.
            let trust = if vanished.trust {
                Trust::Unknown
            } else {
                trust_at_open(&conn)
            };
            // A repair the user was never told of stays part of what this one tells.
            let earlier = earlier_notice(&conn, vanished.notice);
            if !vanished.settings
                && let Some(reindexed) = reindex(&conn, settings_lost, &earlier)?
            {
                crate::verr!(
                    "[SETTINGS] an index of settings.db disagreed with its table, rebuilt"
                );
                return Ok((conn, reindexed));
            }
            let salvaged = salvage(&conn);
            drop(conn);
            rebuild(
                path,
                &salvaged.rows,
                settings_lost || !salvaged.intact,
                trust,
                earlier,
            )
        }
        // Nothing could be read without a connection, not even the earlier notice.
        Err(e) if corruption(&e) => rebuild(path, &[], true, Trust::Unknown, Earlier::Unreadable),
        Err(e) => Err(e),
    }
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
