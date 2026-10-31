use rusqlite::{Connection, params};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// The cap the user picks in the settings, in whole gigabytes.
pub(crate) const MIN_CAPACITY_GB: u8 = 1;
pub(crate) const MAX_CAPACITY_GB: u8 = 6;
pub(crate) const DEFAULT_CAPACITY_GB: u8 = 2;

/// Gigabytes as the menu and the settings print them: binary, like [`format_size`].
pub(crate) const fn capacity_bytes(gb: u8) -> u64 {
    gb as u64 * 1024 * 1024 * 1024
}

/// The cache's size as the menu and the settings page print it: megabytes below a gigabyte,
/// gigabytes from there.
pub(crate) fn format_size(bytes: u64) -> String {
    let gb = capacity_bytes(1);
    if bytes >= gb {
        format!("{:.1} GB", bytes as f64 / gb as f64)
    } else {
        format!("{:.0} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn serialize_size<S: serde::Serializer>(bytes: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format_size(*bytes))
}

/// What the cached files contain: v1 is TIDAL's ciphertext as fetched, v0 was
/// decrypted audio. Bump on any change to the contents - AES-CTR carries no auth
/// tag: a stale entry decodes to noise instead of failing. Wipe, never guess.
const CACHE_FORMAT_VERSION: i64 = 1;

/// Eviction hysteresis: evict until total_size < max_bytes * 0.9.
const EVICTION_FACTOR: f64 = 0.9;

/// Rows taken out of the index (and files unlinked) per step of an eviction pass or a Clear; a
/// large overflow works in bounded batches instead of materializing the whole table.
const EVICTION_BATCH: i64 = 64;

/// Rows a Clear's sweep reads, in one lock section, to know which files are not its to take. A
/// Clear leaves rows only where files would not delete; a Clear that works never reaches this.
const OWNED_MAX: i64 = 4096;

/// Which store of an id a reader judged. Every store of an id lands on the same path; a verdict
/// taken on a read would otherwise hit whatever row the id has once the lock is back. Only the
/// cache mints one; a removal that takes one acts on the row it names and on no other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Generation(i64);

impl Generation {
    /// A ticket no row carries, for a test that needs one.
    #[cfg(test)]
    pub(crate) const NONE: Self = Self(-1);
}

/// What [`lookup_seen`](AudioCache::lookup_seen) found: where the entry is, and which store of the
/// id it is.
pub(crate) struct Seen {
    pub(crate) path: PathBuf,
    pub(crate) generation: Generation,
}

/// What a [`drop_seen`](AudioCache::drop_seen) attempt accomplished. A bare `bool` could
/// not separate "the file would not go and the row was kept" from "the file went, there was
/// no row"; those are opposite answers for a caller asking whether bytes remain on disk.
#[must_use = "a kept file leaves its row behind, which the caller has to account for"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropOutcome {
    /// File gone (removed, or already absent) and its index row with it.
    Dropped,
    /// No row went: there was none, or the id's row is no longer the one the caller judged (then
    /// its file stays too).
    NoRow,
    /// The file would not delete; its row stays and keeps those bytes accounted for.
    FileKept,
}

/// What became of a store request. A bare `Ok` could not separate "in the cache" from
/// "written, then deleted by the eviction this same call triggered", which had the caller
/// logging a successful store for a file already gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOutcome {
    /// Indexed and on disk.
    Kept,
    /// Refused before insert: the file alone exceeds the whole cache. No row and no file
    /// survive, including a row left behind by an earlier failed read.
    TooLarge,
    /// Oversized like [`TooLarge`](Self::TooLarge), but the file would not delete. Indexed
    /// at its true size instead; unindexed it escapes both accounting and eviction, and an
    /// indexed lie lets `lookup_path` serve it. Holds the total over the cap until the file
    /// becomes deletable, which any later eviction pass and store of the same id both retry
    /// (its own store's pass cannot free it, and does not try).
    TooLargeRetained,
    /// Nothing indexed because the cache is off: turned off by the user, or left without an
    /// index. A file staged regardless (a download that began while it was on) is removed.
    Disabled,
    /// The id already has a row, or another download of it is being moved into place: the
    /// entry in place stays and the new download is discarded.
    AlreadyCached,
    /// A Clear took the file between its move and its row: nothing is indexed.
    ClearedMidWrite,
}

/// The right to move one download onto its cache path, held from [`AudioCache::reserve`] to
/// [`AudioCache::commit_row`]. While it stands nothing else stores that id.
#[derive(Debug)]
pub struct Reservation {
    track_id: String,
    path: PathBuf,
}

/// A reservation that gives its id back if dropped still held (a panic, a lock failed at the
/// commit). `commit_row` takes it out and removes the id under the lock: the id leaves `pending`
/// once, and a later reservation of it (refused while this one stands) is never the one removed.
struct Held<'a> {
    cache: &'a Mutex<AudioCache>,
    reservation: Option<Reservation>,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            self.cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pending
                .remove(&reservation.track_id);
        }
    }
}

/// The span of a Clear: `wiping` is set for as long as it lives, whichever way the Clear ends, a
/// panic included. A flag left set would refuse every later store its row, silently.
struct Wiping<'a>(&'a Mutex<AudioCache>);

impl<'a> Wiping<'a> {
    fn begin(cache: &'a Mutex<AudioCache>) -> Self {
        locked(cache).wiping = true;
        Self(cache)
    }
}

impl Drop for Wiping<'_> {
    fn drop(&mut self) {
        locked(self.0).wiping = false;
    }
}

/// Read-held for the length of a move, write-held by a Clear from its first step to its answer: a
/// move waits the Clear out and is indexed after it, instead of landing in a shard the sweep is
/// emptying. Taken BEFORE the cache lock by the Clear, never while holding it by a move, so that a
/// slow rename is waited out without the lock every lookup needs.
static MOVE_GATE: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// Proof that [`MOVE_GATE`] is held for writing: no move is in flight and none starts until it is
/// dropped. Only [`hold_moves_off`] builds one, which keeps [`AudioCache::wipe`], the one function
/// asking for it, from running without the gate.
struct MovesHeldOff(
    #[expect(dead_code, reason = "held for its Drop, which releases the gate")]
    std::sync::RwLockWriteGuard<'static, ()>,
);

/// Taken by [`apply_changes`] ahead of the cache lock, given back once the Clear is answered;
/// first waits for the moves in flight.
fn hold_moves_off() -> MovesHeldOff {
    MovesHeldOff(MOVE_GATE.write().unwrap_or_else(|e| e.into_inner()))
}

impl Reservation {
    /// Takes no cache lock, only the gate a Clear holds for its wipe.
    pub fn move_in(&self, staged: tempfile::NamedTempFile) -> anyhow::Result<()> {
        let _moving = MOVE_GATE.read().unwrap_or_else(|e| e.into_inner());
        AudioCache::persist_file(&self.path, staged)
    }
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Monotonic stamp for the `last_access` LRU sort key: strictly greater than any
/// existing value: a backward wall-clock step cannot make a fresh entry sort
/// older than the entries it was accessed after.
fn next_access_stamp(conn: &Connection, now: i64) -> i64 {
    let max_seen: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(last_access), 0) FROM audio_cache",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    now.max(max_seen + 1)
}

/// Hash a track_id to a hex string for filesystem storage.
fn track_hash(track_id: &str) -> String {
    // Simple FNV-1a hash for fast, well-distributed sharding
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in track_id.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", hash)
}

fn shard_prefix(hash: &str) -> &str {
    &hash[..2]
}

/// The names (track hashes) of the files the index's rows describe, of at most `limit` rows
/// (`-1`: all of them).
fn row_names(conn: &Connection, limit: i64) -> rusqlite::Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT track_id FROM audio_cache LIMIT ?1")?;
    let rows = stmt.query_map(params![limit], |r| r.get::<_, String>(0))?;
    rows.map(|id| id.map(|id| track_hash(&id))).collect()
}

pub struct AudioCache {
    /// `None`: disabled mode (no openable index location); lookups miss and
    /// stores are dropped.
    conn: Option<Connection>,
    audio_dir: PathBuf,
    max_bytes: u64,
    /// Running total of `file_size` across the index, maintained under the
    /// cache lock, and eviction never re-sums the table. Seeded from the DB at
    /// open; a crash just re-seeds next boot.
    current_size: u64,
    /// The user's switch. Off, the index stays open; what it holds is still counted, and is
    /// found again once the switch is back on.
    enabled: bool,
    /// The `seq` of the last [`state`](Self::state) snapshot.
    snapshots: u64,
    /// Told after every change to what the page shows; `None` on every cache but the global one.
    on_change: Option<mpsc::SyncSender<()>>,
    /// What the last Clear that failed left undone, until a Clear succeeds. A snapshot taken
    /// later by a store carries it too (state, not an event).
    last_failure: Option<Failed>,
    /// Why the last eviction pass left the cache over its cap, `None` if it did not. Only said
    /// while the cache is still over the cap: see [`untrimmed`](Self::untrimmed).
    trim_blocked: Option<&'static str>,
    /// Ids whose download is being moved onto its cache path, outside the lock. A path with no
    /// row is only ever written by the holder of its reservation.
    pending: std::collections::HashSet<String>,
    /// Ids whose row an eviction took out of the index and whose file is being unlinked, outside
    /// the lock. Like `pending`, a path with no row that somebody owns: nothing reserves, drops or
    /// serves it until the unlink is done and the id is released.
    doomed: std::collections::HashSet<String>,
    /// A Clear is running: a download that lands now is not indexed (its file is the Clear's to
    /// take), or the row would describe a file the Clear is about to remove.
    wiping: bool,
    /// Bytes of files whose row an eviction took out and could not put back (the index refused
    /// the INSERT): on disk, outside the cap, until a Clear or the next start finds them.
    stranded: u64,
    /// The [`Generation`] the next row takes: above every one in the index at open, never reused.
    next_generation: i64,
}

/// The eviction pass a store's row owes, to run once the lock is let go.
struct Owed {
    keep: String,
    unreclaimable: u64,
}

/// A row an eviction took out of the index, with everything needed to put it back should its file
/// refuse to go.
struct Doomed {
    id: String,
    format: String,
    size: i64,
    created_at: i64,
    last_access: i64,
    access_count: i64,
    generation: i64,
    path: PathBuf,
}

/// One lock section of a pass over the shared cache. The lock is taken per step and is free while
/// the files are unlinked.
fn locked(cache: &Mutex<AudioCache>) -> std::sync::MutexGuard<'_, AudioCache> {
    cache.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where a pass stands in the LRU order, `(last_access, track_id)`: a total order, so that rows
/// tied on a stamp (a `put_back` restoring one a later insert has reused) are neither skipped
/// nor met twice across two batches.
struct Cursor {
    last_access: i64,
    id: String,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            last_access: i64::MIN,
            id: String::new(),
        }
    }
}

/// What a step of a pass is after.
enum Goal<'a> {
    /// The LRU rows that bring the size under the target. A pass starts only over the cap
    /// (`first`), then drains to the target. `keep` is never a candidate; `unreclaimable` bytes
    /// are counted in the size and no row can free them.
    Trim {
        keep: Option<&'a str>,
        unreclaimable: u64,
        first: bool,
    },
    /// Every row, the Clear's.
    All,
}

/// What the settings page shows of the cache.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CacheState {
    pub(crate) seq: u64,
    pub(crate) enabled: bool,
    pub(crate) max_bytes: u64,
    /// Sent as [`format_size`] text: the page shows it as is, with no second copy of the rule.
    #[serde(rename = "usedText", serialize_with = "serialize_size")]
    pub(crate) used_bytes: u64,
    /// Whether the index opened. Fixed for the process. With it closed nothing is cached, and
    /// the switch and the size say nothing true.
    pub(crate) available: bool,
    pub(crate) last_failure: Option<Failed>,
}

/// The changes the user queues from the settings or the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Request {
    Switch,
    Resize,
    Clear,
}

/// The actions whose failure the page reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    Clear,
    Resize,
}

/// A Clear or a Resize that did not complete, as the page is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Failed {
    pub(crate) action: Action,
    pub(crate) reason: &'static str,
}

/// What stopped a Clear or a Resize. The text is for the log; the page gets the phrase.
#[derive(Debug)]
pub enum CacheError {
    /// The cache directory could not be removed.
    Disk(std::io::Error),
    /// The index refused a statement.
    Index(rusqlite::Error),
}

impl CacheError {
    /// The only failure text sent out of the process, a constant: no path or OS message travels
    /// with it to the page.
    pub(crate) fn phrase(&self) -> &'static str {
        match self {
            Self::Disk(_) => HELD,
            Self::Index(_) => INDEX,
        }
    }
}

const HELD: &str =
    "another program may hold a file in the cache folder, or the disk refuses to remove it";
const INDEX: &str = "the cache index could not be updated";

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disk(e) => write!(f, "could not clear the audio cache directory: {e}"),
            Self::Index(e) => write!(f, "the cache index refused the change: {e}"),
        }
    }
}

impl std::error::Error for CacheError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Disk(e) => Some(e),
            Self::Index(e) => Some(e),
        }
    }
}

impl From<rusqlite::Error> for CacheError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Index(e)
    }
}

impl AudioCache {
    /// Open (or create) the audio cache in the given data directory. A cache already over
    /// `max_bytes` is left for [`settle_after_open`](Self::settle_after_open) to evict.
    pub fn open_with_capacity(data_dir: &Path, max_bytes: u64) -> rusqlite::Result<Self> {
        let cache_dir = data_dir.join("cache");
        let audio_dir = cache_dir.join("audio");
        let db_path = cache_dir.join("index.db");

        fs::create_dir_all(&audio_dir).ok();

        let conn = Connection::open(&db_path)?;

        // WAL mode for concurrent reads
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS audio_cache (
                track_id     TEXT PRIMARY KEY,
                format       TEXT NOT NULL,
                file_size    INTEGER NOT NULL,
                created_at   INTEGER NOT NULL,
                last_access  INTEGER NOT NULL,
                access_count INTEGER DEFAULT 1
            );
            CREATE INDEX IF NOT EXISTS idx_audio_cache_lru
                ON audio_cache (last_access, track_id);
            DROP INDEX IF EXISTS idx_audio_cache_last_access;",
        )?;
        let next_generation = Self::open_generations(&conn)?;

        Self::migrate_format(&conn, &audio_dir)?;
        Self::sweep_orphaned_staging(&audio_dir);
        Self::sweep_orphaned_files(&conn, &audio_dir);

        // Seed the running size once; every mutation keeps it current thereafter.
        let current_size = conn
            .query_row(
                "SELECT COALESCE(SUM(file_size), 0) FROM audio_cache",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0) as u64;

        // No eviction here: a cache over its cap (the cap lowered by a session that ended before
        // its own pass did) is trimmed by [`settle_after_open`] once the cache is shared. The init
        // the lock waits behind never unlinks.
        Ok(Self {
            conn: Some(conn),
            audio_dir,
            max_bytes,
            current_size,
            enabled: true,
            snapshots: 0,
            on_change: None,
            last_failure: None,
            trim_blocked: None,
            pending: Default::default(),
            doomed: Default::default(),
            wiping: false,
            stranded: 0,
            next_generation,
        })
    }

    /// Give an index from before generations the column (its rows all take generation 0), and
    /// return the next one, above the highest in the index; no row a reader sees can share one
    /// with an earlier store. The format version stays, a bump would wipe the cache.
    fn open_generations(conn: &Connection) -> rusqlite::Result<i64> {
        let has_column: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('audio_cache') WHERE name = 'gen'",
            [],
            |row| row.get(0),
        )?;
        if !has_column {
            conn.execute(
                "ALTER TABLE audio_cache ADD COLUMN gen INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        conn.query_row(
            "SELECT COALESCE(MAX(gen), 0) + 1 FROM audio_cache",
            [],
            |row| row.get(0),
        )
    }

    fn take_generation(&mut self) -> i64 {
        let generation = self.next_generation;
        self.next_generation += 1;
        generation
    }

    /// The eviction `open_with_capacity` owes a cache left over its cap: run once the global cache
    /// exists, unlinking with the lock free between steps.
    pub(crate) fn settle_after_open(cache: &Mutex<AudioCache>) {
        let outcome = Self::drain(cache, None, 0, &|p| fs::remove_file(p));
        let _ = locked(cache).finish_pass(outcome.map_err(|e| {
            crate::verr!("[CACHE]  Eviction at open failed: {e}");
            e
        }));
    }

    /// Drop every cached file when the on-disk generation isn't the one this
    /// build reads. Runs before `current_size` is seeded: the total reflects
    /// the wipe. `user_version` defaults to 0 on a pre-versioning database,
    /// which is exactly the plaintext generation we must discard.
    fn migrate_format(conn: &Connection, audio_dir: &Path) -> rusqlite::Result<()> {
        let on_disk: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if on_disk == CACHE_FORMAT_VERSION {
            return Ok(());
        }

        let dropped: i64 = conn
            .query_row("SELECT COUNT(*) FROM audio_cache", [], |row| row.get(0))
            .unwrap_or(0);

        // Fail closed: committing the version after a failed wipe strands the
        // survivors outside the size cap for good, and never retries.
        if audio_dir.exists()
            && let Err(e) = fs::remove_dir_all(audio_dir)
        {
            crate::vprintln!("[CACHE]  Format v{on_disk} -> v{CACHE_FORMAT_VERSION} deferred: {e}");
            return Ok(());
        }
        // The staging path recreates it on demand; a failure only defers staging.
        fs::create_dir_all(audio_dir).ok();
        conn.execute("DELETE FROM audio_cache", [])?;
        conn.pragma_update(None, "user_version", CACHE_FORMAT_VERSION)?;

        crate::vprintln!(
            "[CACHE]  Format v{on_disk} -> v{CACHE_FORMAT_VERSION}: dropped {dropped} entries"
        );
        Ok(())
    }

    /// Delete the files no row describes. An eviction or a Clear cut short (a crash, a kill) after
    /// taking its rows out leaves them, outside the cap and invisible to eviction. Run at open, before
    /// the cache is shared: a shard file is a track's hash, and only a row gives it a reason to exist.
    fn sweep_orphaned_files(conn: &Connection, audio_dir: &Path) {
        let Ok(known) = row_names(conn, -1) else {
            return;
        };
        let Ok(shards) = fs::read_dir(audio_dir) else {
            return;
        };
        let mut removed = 0u32;
        for shard in shards.flatten() {
            if !shard.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Ok(files) = fs::read_dir(shard.path()) else {
                continue;
            };
            for file in files.flatten() {
                let name = file.file_name().to_string_lossy().into_owned();
                let is_entry = name.len() == 16 && name.bytes().all(|b| b.is_ascii_hexdigit());
                if is_entry && !known.contains(&name) && fs::remove_file(file.path()).is_ok() {
                    removed += 1;
                }
            }
        }
        if removed > 0 {
            crate::vprintln!("[CACHE]  Removed {removed} file(s) no row described");
        }
    }

    /// Delete staging files orphaned by an unclean exit: they sit at the audio dir
    /// root (an indexed file always lives inside a 2-hex shard): neither the index
    /// nor eviction can see them and they would hold disk outside the size cap.
    fn sweep_orphaned_staging(audio_dir: &Path) {
        let Ok(entries) = fs::read_dir(audio_dir) else {
            return;
        };
        let mut removed = 0u32;
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_file()) && fs::remove_file(entry.path()).is_ok()
            {
                removed += 1;
            }
        }
        if removed > 0 {
            crate::vprintln!("[CACHE]  Removed {removed} orphaned staging file(s)");
        }
    }

    /// A cache with no backing index, for when no location can host
    /// `index.db`: every lookup misses and stores are dropped. Infallible so
    /// the AUDIO_CACHE LazyLock init can never panic (and thus never poison).
    pub fn disabled(data_dir: &Path, max_bytes: u64) -> Self {
        Self {
            conn: None,
            audio_dir: data_dir.join("cache").join("audio"),
            max_bytes,
            current_size: 0,
            enabled: true,
            snapshots: 0,
            on_change: None,
            last_failure: None,
            trim_blocked: None,
            pending: Default::default(),
            doomed: Default::default(),
            wiping: false,
            stranded: 0,
            next_generation: 1,
        }
    }

    /// Have the page told after every change to what it shows. Only the global cache is
    /// watched; a test's own never reaches the page.
    pub(crate) fn watch(&mut self, on_change: mpsc::SyncSender<()>) {
        self.on_change = Some(on_change);
    }

    /// What the page shows has changed. Called by the methods that change it, under the lock and
    /// without waiting: a full slot already means a snapshot is owed, and the one taken for it
    /// sees this change too.
    fn changed(&self) {
        if let Some(on_change) = &self.on_change {
            // Full: coalesced. Disconnected: the publisher never exits, and a lost page update
            // is not worth a log line per mutation.
            let _ = on_change.try_send(());
        }
    }

    /// The user's switch, live. Off serves and stores nothing but deletes nothing either;
    /// emptying the cache is the Clear button's job.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        // Unconditionally, as the page holds the click until a snapshot echoes it.
        self.changed();
    }

    /// Record how a queued change ended, and tell the page; after a failed Clear, which moves no
    /// size, nothing else would. Only a Clear is kept as a failure: it leaves nothing to measure.
    /// A Resize that did not complete shows through [`untrimmed`](Self::untrimmed).
    pub(crate) fn settle(&mut self, request: Request, outcome: Result<(), CacheError>) {
        match (request, outcome) {
            (Request::Clear, Err(e)) => {
                let failed = Failed {
                    action: Action::Clear,
                    reason: e.phrase(),
                };
                // The same failure again is what the page already shows, and `verr!` is budgeted.
                if self.last_failure == Some(failed) {
                    crate::vprintln!("[CACHE]  {request:?} failed again: {e}");
                } else {
                    crate::verr!("[CACHE]  {request:?} failed: {e}");
                }
                self.last_failure = Some(failed);
            }
            (Request::Clear, Ok(())) => self.last_failure = None,
            (_, Err(e)) => crate::verr!("[CACHE]  {request:?} failed: {e}"),
            (_, Ok(())) => {}
        }
        self.changed();
    }

    /// Why the cache is over its cap, while it is. The last pass says why it stopped; whether
    /// that still holds is read from the sizes. A drop, a raised cap or a clear that brings the
    /// cache under needs no word of its own. Every path that pushes the size over the cap
    /// (a store, a lowered cap, the open) runs a pass right after, which restates the reason.
    fn untrimmed(&self) -> Option<&'static str> {
        // Bytes no row accounts for (no size comparison can show them) are said whatever the size.
        if self.stranded > 0 {
            return Some(INDEX);
        }
        self.trim_blocked
            .filter(|_| self.current_size > self.max_bytes)
    }

    /// A snapshot for the settings page. Each one is numbered after the last, all under the
    /// cache lock; two that cross on their way to the page are told apart by `seq`.
    pub(crate) fn state(&mut self) -> CacheState {
        self.snapshots += 1;
        CacheState {
            seq: self.snapshots,
            enabled: self.enabled,
            max_bytes: self.max_bytes,
            used_bytes: self.current_size,
            available: self.conn.is_some(),
            last_failure: self
                .untrimmed()
                .map(|reason| Failed {
                    action: Action::Resize,
                    reason,
                })
                .or(self.last_failure),
        }
    }

    /// On-disk path for a track. Depends only on the (fixed) audio dir, letting the
    /// caller compute it under a brief lock and then write the file without
    /// holding the global cache lock.
    pub(crate) fn file_path(&self, track_id: &str) -> PathBuf {
        let hash = track_hash(track_id);
        let shard = shard_prefix(&hash);
        self.audio_dir.join(shard).join(&hash)
    }

    /// Check if a track exists in the index and return its file path; never while the cache is
    /// turned off. It reads the index only (no disk), which suits a short lock.
    pub fn lookup_path(&self, track_id: &str) -> Option<PathBuf> {
        if !self.enabled {
            return None;
        }
        let conn = self.conn.as_ref()?;
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM audio_cache WHERE track_id = ?1",
                params![track_id],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if exists {
            Some(self.file_path(track_id))
        } else {
            None
        }
    }

    /// [`lookup_path`](Self::lookup_path) for a caller that may act on what it finds: the entry's
    /// path and the generation of the row it was read from.
    pub(crate) fn lookup_seen(&self, track_id: &str) -> Option<Seen> {
        if !self.enabled {
            return None;
        }
        let generation = self.row_generation(track_id)?;
        Some(Seen {
            path: self.file_path(track_id),
            generation,
        })
    }

    /// Reads the index only, no disk.
    fn row_generation(&self, track_id: &str) -> Option<Generation> {
        let conn = self.conn.as_ref()?;
        conn.query_row(
            "SELECT gen FROM audio_cache WHERE track_id = ?1",
            params![track_id],
            |r| r.get(0),
        )
        .ok()
        .map(Generation)
    }

    /// Remove an orphaned index entry (file missing from disk), only while the id's row is still
    /// the `seen` one: a store since then is another entry, with its own file. Reports whether a
    /// row went away; a caller cannot mistake a silent no-op for a deletion.
    pub(crate) fn remove_orphan(&mut self, track_id: &str, seen: Generation) -> bool {
        self.remove_row(track_id, Some(seen))
    }

    /// Delete the id's row (only if it is of generation `only`, when given) and take its bytes out
    /// of the running size.
    fn remove_row(&mut self, track_id: &str, only: Option<Generation>) -> bool {
        let Some(conn) = &self.conn else { return false };
        let want = only.map(|g| g.0);
        let prev_size: i64 = conn
            .query_row(
                "SELECT file_size FROM audio_cache WHERE track_id = ?1 AND (?2 IS NULL OR gen = ?2)",
                params![track_id, want],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let deleted = conn
            .execute(
                "DELETE FROM audio_cache WHERE track_id = ?1 AND (?2 IS NULL OR gen = ?2)",
                params![track_id, want],
            )
            .unwrap_or(0);
        if deleted > 0 {
            self.current_size = self.current_size.saturating_sub(prev_size as u64);
            self.changed();
        }
        deleted > 0
    }

    /// Drop an entry the caller judged, while the id's row is still the `seen` one: file first,
    /// row once the file is gone, all under one lock. Losing the row while the file survives would
    /// strand the bytes outside `max_bytes`.
    pub(crate) fn drop_seen(&mut self, track_id: &str, seen: Generation) -> DropOutcome {
        if self.row_generation(track_id) != Some(seen) {
            return DropOutcome::NoRow;
        }
        self.drop_entry_with(track_id, &|p| fs::remove_file(p))
    }

    /// [`drop_seen`](Self::drop_seen) without the comparison, with the unlink passed in for tests
    /// to fail it: for a caller that decided under the lock it holds, on the row it holds.
    fn drop_entry_with(
        &mut self,
        track_id: &str,
        remove: &dyn Fn(&Path) -> std::io::Result<()>,
    ) -> DropOutcome {
        let path = self.file_path(track_id);
        match remove(&path) {
            Ok(()) => {}
            // Already gone: the row is the orphan.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                crate::vprintln!("[CACHE]  Keeping the index row for {track_id}: {e}");
                return DropOutcome::FileKept;
            }
        }
        if self.remove_row(track_id, None) {
            DropOutcome::Dropped
        } else {
            DropOutcome::NoRow
        }
    }

    /// Update access metadata after a successful cache read.
    pub fn touch(&self, track_id: &str) {
        let Some(conn) = &self.conn else { return };
        let stamp = next_access_stamp(conn, now_epoch());
        let _ = conn.execute(
            "UPDATE audio_cache SET last_access = ?1, access_count = access_count + 1 WHERE track_id = ?2",
            params![stamp, track_id],
        );
    }

    /// Where a download stages its ciphertext, or `None` when the cache is off: the file would
    /// never be indexed, and staging it would write and delete a full track for nothing.
    /// Staging inside the audio dir keeps [`Reservation::move_in`] a same-filesystem rename, and
    /// handing back the directory lets the caller create the file without the cache lock held.
    pub fn staging_dir(&self) -> Option<PathBuf> {
        (self.conn.is_some() && self.enabled).then(|| self.audio_dir.clone())
    }

    /// Move a completed staging file to its cache path (an atomic rename).
    fn persist_file(path: &Path, staged: tempfile::NamedTempFile) -> anyhow::Result<()> {
        let shard_dir = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cache path has no parent: {}", path.display()))?;
        fs::create_dir_all(shard_dir)?;
        staged.persist(path)?;
        Ok(())
    }

    /// First half of a store, under the lock: until the reservation ends no other store, drop or
    /// eviction of the id can touch its path (no row for eviction to see, no second reservation).
    /// A refusal returns the outcome and the caller drops its staging file.
    pub fn reserve(&mut self, track_id: &str) -> Result<Reservation, StoreOutcome> {
        if self.conn.is_none() || !self.enabled {
            return Err(StoreOutcome::Disabled);
        }
        // Re-checks the rule `open_cipher_sink` applies when the download starts: a row may have
        // appeared since, and renaming over its file would swap what it describes for bytes it
        // never measured. A doomed id has no row, but its old file about to be unlinked would
        // take the new bytes with it.
        if self.lookup_path(track_id).is_some()
            || self.doomed.contains(track_id)
            || !self.pending.insert(track_id.to_owned())
        {
            return Err(StoreOutcome::AlreadyCached);
        }
        Ok(Reservation {
            track_id: track_id.to_owned(),
            path: self.file_path(track_id),
        })
    }

    /// Second half of a store, under the lock; `moved` is how the unlocked move ended, and the
    /// reservation ends here either way. The row goes in only if the switch is on, no Clear runs
    /// and the file is on its path; the pass it owes comes back for the caller to run lock-free.
    fn commit_row(
        &mut self,
        reservation: Reservation,
        format: &str,
        file_size: u64,
        moved: anyhow::Result<()>,
    ) -> anyhow::Result<(StoreOutcome, Option<Owed>)> {
        let Reservation { track_id, path } = reservation;
        self.pending.remove(&track_id);
        moved?;
        if self.conn.is_none() || !self.enabled {
            // No row, and no other writer of this path while it was reserved: the file is the
            // download's own, never a live entry's.
            let _ = fs::remove_file(&path);
            return Ok((StoreOutcome::Disabled, None));
        }
        // The file was moved before the Clear took the gate and is still there, for its sweep: a
        // row now would describe a file the Clear is about to remove.
        if self.wiping {
            return Ok((StoreOutcome::ClearedMidWrite, None));
        }
        if matches!(fs::metadata(&path), Err(e) if e.kind() == std::io::ErrorKind::NotFound) {
            return Ok((StoreOutcome::ClearedMidWrite, None));
        }
        self.insert_row(&track_id, format, file_size, &|p| fs::remove_file(p))
    }

    /// Take a finished download into `cache`: [`reserve`](Self::reserve), the move into place
    /// with the lock released (the multi-megabyte write was the staging file's, done by the
    /// download itself; what remains is a rename, which can stall behind a scanner), then
    /// [`commit_row`](Self::commit_row). Dropping the staging file unlinks it outside the lock.
    pub fn store_into(
        cache: &Mutex<AudioCache>,
        track_id: &str,
        format: &str,
        staged: tempfile::NamedTempFile,
        file_size: u64,
    ) -> anyhow::Result<StoreOutcome> {
        let lock = || {
            cache
                .lock()
                .map_err(|_| anyhow::anyhow!("audio cache lock poisoned"))
        };
        let reservation = match lock()?.reserve(track_id) {
            Ok(reservation) => reservation,
            Err(refused) => return Ok(refused),
        };
        // From here every way out, a panic included, gives the id back unless `commit_row` took it.
        let mut held = Held {
            cache,
            reservation: Some(reservation),
        };
        let moved = held.reservation.as_ref().map_or_else(
            || Err(anyhow::anyhow!("reservation already released")),
            |reservation| reservation.move_in(staged),
        );
        let (outcome, owed) = {
            let mut locked = lock()?;
            match held.reservation.take() {
                Some(reservation) => locked.commit_row(reservation, format, file_size, moved)?,
                None => return Err(anyhow::anyhow!("reservation already released")),
            }
        };
        // The pass the row owes unlinks with the lock free: lookups and stores go on meanwhile.
        if let Some(owed) = owed {
            Self::evict(cache, Some(&owed.keep), owed.unreclaimable, &|p| {
                fs::remove_file(p)
            })?;
        }
        Ok(outcome)
    }

    /// Take a completed ciphertext into the cache, off the calling thread. The caller owes
    /// the checks this cannot make: that the download completed, and that no decoder
    /// rejected these bytes.
    pub fn store_ciphertext_detached(
        track_id: String,
        format: String,
        staged: tempfile::NamedTempFile,
        len: u64,
    ) {
        std::thread::spawn(move || {
            match Self::store_into(&crate::state::AUDIO_CACHE, &track_id, &format, staged, len) {
                Ok(StoreOutcome::Kept) => crate::vprintln!(
                    "[CACHE]  Stored: {} ({}, encrypted)",
                    track_id,
                    crate::player::format_bytes(len)
                ),
                // Reported with both sizes, ungated, by the cache itself.
                Ok(StoreOutcome::TooLarge) => {}
                // Ungated (disk held over the cap, no other channel), and retried
                // only by a later eviction pass or store of this id.
                Ok(StoreOutcome::TooLargeRetained) => crate::verr!(
                    "[CACHE]  Oversized entry could not be removed, indexed at {}: {}",
                    crate::player::format_bytes(len),
                    track_id
                ),
                Ok(StoreOutcome::Disabled) => crate::vprintln!(
                    "[CACHE]  Store discarded (cache turned off mid-write): {track_id}"
                ),
                Ok(StoreOutcome::AlreadyCached) => {
                    crate::vprintln!("[CACHE]  Store discarded (already cached): {track_id}")
                }
                Ok(StoreOutcome::ClearedMidWrite) => crate::vprintln!(
                    "[CACHE]  Store discarded (cache cleared mid-write): {track_id}"
                ),
                Err(e) => crate::vprintln!("[CACHE]  Store failed: {e}"),
            }
        });
    }

    /// Retire an entry from a thread that must not wait for the cache lock.
    ///
    /// Both decode-failure sites run on the player control thread, which cannot afford this
    /// mutex: its holders (a store's row, a step of an eviction or a Clear) can stall behind the
    /// disk, and an inline drop parks the transport as long. Skipping is not open to them either,
    /// nothing else retiring an entry whose header sniffs fine and whose bitstream does not
    /// decode: every successful read touches `last_access`, so a failing entry keeps re-arming
    /// its own recency and eviction never reaches it. Late is correct here, never is not.
    ///
    /// Detached like a store. The failure comes up to a whole track after the read that judged the
    /// entry, room for a Clear and a store of the same id: `seen` is the generation the load read,
    /// and a row of any other generation is left alone.
    pub(crate) fn drop_entry_detached(track_id: String, seen: Generation) {
        std::thread::spawn(move || {
            let _ = Self::drop_after_decode_failure(&crate::state::AUDIO_CACHE, &track_id, seen);
        });
    }

    /// The detached thread's body, split out to run against a cache of its own in tests.
    fn drop_after_decode_failure(
        cache: &Mutex<AudioCache>,
        track_id: &str,
        seen: Generation,
    ) -> DropOutcome {
        let Ok(mut cache) = cache.lock() else {
            crate::vprintln!("[CACHE]  Lock poisoned, entry left indexed: {track_id}");
            return DropOutcome::NoRow;
        };
        let outcome = cache.drop_seen(track_id, seen);
        if outcome == DropOutcome::Dropped {
            crate::vprintln!("[CACHE]  Dropped after a decode failure: {track_id}");
        }
        outcome
    }

    /// How far an overflow eviction drains, never a per-entry size limit. Eviction starts
    /// only once `current_size` passes `max_bytes`; this is just the hysteresis floor.
    fn eviction_target(&self) -> u64 {
        (self.max_bytes as f64 * EVICTION_FACTOR) as u64
    }

    /// Record an already-written track in the index: index insert only, no large I/O. The
    /// eviction pass the row owes comes back for the caller to run with the lock free. `remove`
    /// is the unlink an oversized file goes through, passed in for tests to fail it.
    fn insert_row(
        &mut self,
        track_id: &str,
        format: &str,
        file_size: u64,
        remove: &dyn Fn(&Path) -> std::io::Result<()>,
    ) -> anyhow::Result<(StoreOutcome, Option<Owed>)> {
        if self.conn.is_none() {
            return Ok((StoreOutcome::Disabled, None));
        }
        // Admission control, before the row exists. The bound is `max_bytes`, not the
        // eviction target: eviction only starts above `max_bytes`, and the target merely says
        // how far a pass that started should drain. The one size no pass can resolve is an
        // entry bigger than the whole cache, holding `current_size` over the cap with nothing
        // else left to evict.
        //
        // `drop_entry_with`, not a bare `remove_file`: a row for this id can already exist, an
        // entry that failed to decrypt being kept on purpose (`CacheReadError::Unreadable`).
        let mut unremovable = false;
        if file_size > self.max_bytes {
            match self.drop_entry_with(track_id, remove) {
                // Ungated (a track that will never cache has no other channel), and the one
                // site holding both sizes.
                DropOutcome::Dropped | DropOutcome::NoRow => {
                    crate::verr!(
                        "[CACHE]  Not cached, {} exceeds the {} cache: {}",
                        crate::player::format_bytes(file_size),
                        crate::player::format_bytes(self.max_bytes),
                        track_id
                    );
                    return Ok((StoreOutcome::TooLarge, None));
                }
                // The bytes are there and will not go. Indexing them at their real size is
                // the only honest option; unindexed they escape accounting and eviction both.
                DropOutcome::FileKept => unremovable = true,
            }
        }
        // Unreachable, the disabled case having returned above; kept because `conn`'s
        // immutable borrow must follow `drop_entry_with`'s `&mut self`.
        let generation = self.take_generation();
        let Some(conn) = &self.conn else {
            return Ok((StoreOutcome::Disabled, None));
        };
        let now = now_epoch();
        let stamp = next_access_stamp(conn, now);
        // INSERT OR REPLACE overwrites an existing row; the running total moves
        // by the size delta, not the full new size.
        let prev_size: i64 = conn
            .query_row(
                "SELECT file_size FROM audio_cache WHERE track_id = ?1",
                params![track_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        conn.execute(
            "INSERT OR REPLACE INTO audio_cache (track_id, format, file_size, created_at, last_access, access_count, gen)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)",
            params![track_id, format, file_size as i64, now, stamp, generation],
        )?;
        self.current_size = (self.current_size + file_size).saturating_sub(prev_size as u64);
        self.changed();

        // An undeletable oversized row is the one thing eviction can never free. Every other
        // store hands over zero, keeping its own bytes in the measurement.
        let owed = Owed {
            keep: track_id.to_owned(),
            unreclaimable: if unremovable { file_size } else { 0 },
        };
        Ok((
            if unremovable {
                StoreOutcome::TooLargeRetained
            } else {
                StoreOutcome::Kept
            },
            Some(owed),
        ))
    }

    /// A Clear, [`MOVE_GATE`] held for writing (the proof): its rows go through an eviction's steps
    /// (claim, unlink lock-free, settle), then what no row describes is swept out of `audio/`, lock-free
    /// too. No folder is renamed or removed wholesale: a download's open file cannot fail the Clear.
    ///
    /// A file that will not delete puts its row back (counted, fail closed) and the Clear ends in
    /// its first refusal after freeing the rest; a second Clear retries only what is left. Staging
    /// files at the root of `audio/` stay: a download in flight moves in after the Clear, at the gate.
    fn wipe(
        cache: &Mutex<AudioCache>,
        _: &MovesHeldOff,
        remove: &dyn Fn(&Path) -> std::io::Result<()>,
    ) -> Result<(), CacheError> {
        let _wiping = Wiping::begin(cache);
        let mut cursor = Cursor::default();
        let (mut cleared, mut freed) = (0usize, 0i64);
        let mut refused = None;
        loop {
            let batch = locked(cache).claim(Goal::All, &mut cursor)?;
            if batch.is_empty() {
                break;
            }
            let settled = Self::unlink_batch(cache, batch, remove);
            cleared += settled.evicted.len();
            freed += settled.evicted.iter().map(|(_, size)| size).sum::<i64>();
            if refused.is_none() {
                refused = settled.kept.into_iter().next().map(|(_, e)| e);
            }
        }
        // The rows left (the refused, and any a concurrent pass put back) and the ids a pass has
        // out for unlinking, read in one section: their files are not the sweep's to take. No row
        // can be added from here, a download landing now being refused its row (`wiping`).
        let (audio_dir, owned) = {
            let cache = locked(cache);
            (cache.audio_dir.clone(), cache.owned_names()?)
        };
        let swept = Self::sweep_unowned(&audio_dir, owned.as_ref());
        crate::vprintln!(
            "[CACHE]  Cleared {cleared} entries ({:.1} MB)",
            freed as f64 / (1024.0 * 1024.0)
        );
        match (refused, swept) {
            (Some(e), _) | (None, Err(e)) => Err(CacheError::Disk(e)),
            (None, Ok(())) => Ok(()),
        }
    }

    /// The names (track hashes) of the files a row or a pass in flight accounts for; `None` when
    /// more than [`OWNED_MAX`] rows are left, which only a Clear that could delete next to nothing
    /// sees: the section stays bounded and the shards are not swept.
    fn owned_names(&self) -> Result<Option<std::collections::HashSet<String>>, CacheError> {
        let mut owned: std::collections::HashSet<String> =
            self.doomed.iter().map(|id| track_hash(id)).collect();
        if let Some(conn) = &self.conn {
            let rows = row_names(conn, OWNED_MAX + 1)?;
            if rows.len() as i64 > OWNED_MAX {
                return Ok(None);
            }
            owned.extend(rows);
        }
        Ok(Some(owned))
    }

    /// Delete what `owned` does not name from the shard directories of `audio_dir`, then the
    /// shards left empty; staging files at the root stay, and without `owned` nothing is swept.
    /// No lock is held and files go by name: one that will not delete costs the Clear only itself,
    /// and a file left in a shard is an error (bytes on disk, outside the cap).
    fn sweep_unowned(
        audio_dir: &Path,
        owned: Option<&std::collections::HashSet<String>>,
    ) -> std::io::Result<()> {
        let entries = match fs::read_dir(audio_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut first = None;
        let remove = |path: &Path, is_dir: bool| match if is_dir {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        } {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Some(e),
            _ => None,
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            // The files at the root are downloads' staging files, live in this session: not the
            // Clear's to take (a crash's are `sweep_orphaned_staging`'s, at open).
            if !kind.is_dir() {
                continue;
            }
            let Some(owned) = owned else {
                continue;
            };
            let shard = entry.path();
            match fs::read_dir(&shard) {
                Ok(files) => {
                    for file in files.flatten() {
                        let named = file.file_name();
                        if named.to_str().is_some_and(|name| owned.contains(name)) {
                            continue;
                        }
                        let is_dir = file.file_type().is_ok_and(|t| t.is_dir());
                        if let Some(e) = remove(&file.path(), is_dir) {
                            first.get_or_insert(e);
                        }
                    }
                }
                Err(e) => {
                    first.get_or_insert(e);
                }
            }
            // Not recursive: a shard that still holds an owned file stays.
            let _ = fs::remove_dir(&shard);
        }
        first.map_or(Ok(()), Err)
    }

    /// The running total of cached bytes (maintained incrementally, not re-summed).
    pub fn total_size(&self) -> u64 {
        self.current_size
    }

    /// Run an eviction pass ([`drain`](Self::drain), which says what `keep` and `unreclaimable`
    /// mean) and keep why it left the cache over its cap, for [`untrimmed`](Self::untrimmed).
    pub(crate) fn evict(
        cache: &Mutex<AudioCache>,
        keep: Option<&str>,
        unreclaimable: u64,
        remove: &dyn Fn(&Path) -> std::io::Result<()>,
    ) -> Result<(), CacheError> {
        let outcome = Self::drain(cache, keep, unreclaimable, remove);
        locked(cache).finish_pass(outcome)
    }

    /// The end of a pass, in one lock section with whatever the caller says next: why the cache
    /// is left over its cap, if it is. Read from the sizes as they are now, whatever the other
    /// passes did meanwhile; each pass ends by restating it and the last to end has the last word.
    fn finish_pass(&mut self, outcome: Result<(), CacheError>) -> Result<(), CacheError> {
        // A pass that stalls on held files is Ok; a store never fails for one.
        let blocked = match &outcome {
            Err(e) => Some(e.phrase()),
            Ok(()) if self.current_size > self.max_bytes => Some(HELD),
            Ok(()) => None,
        };
        if self.trim_blocked != blocked {
            self.trim_blocked = blocked;
            // The page may have been told mid-pass, without the reason.
            self.changed();
        }
        outcome
    }

    /// Evict LRU entries until the running size is under `max_bytes * EVICTION_FACTOR`, oldest
    /// first via the LRU index, in bounded batches of three steps: [`claim`](Self::claim) (under
    /// the lock), unlink (lock free), [`settle_unlinks`](Self::settle_unlinks) (under the lock).
    ///
    /// `keep` is the row [`insert_row`](Self::insert_row) just inserted, never a candidate here; no
    /// store is undone by the eviction it triggered. `unreclaimable` is how many of its bytes no
    /// pass could ever free; counted into the total, they hold it above a mark no candidate can
    /// reach and the pass empties the cache trying. Only [`TooLargeRetained`](StoreOutcome::TooLargeRetained)
    /// sets it (an oversized file that would not delete); an ordinary store passes zero, its row
    /// being out of scope for this pass yet evictable by the next. A pass no store triggered (a
    /// lowered cap) has neither.
    fn drain(
        cache: &Mutex<AudioCache>,
        keep: Option<&str>,
        unreclaimable: u64,
        remove: &dyn Fn(&Path) -> std::io::Result<()>,
    ) -> Result<(), CacheError> {
        // Candidates are read past the last one examined; a row put back after a failed unlink
        // is not met again by this pass.
        let mut cursor = Cursor::default();
        let mut first = true;
        loop {
            let goal = Goal::Trim {
                keep,
                unreclaimable,
                first,
            };
            let batch = locked(cache).claim(goal, &mut cursor)?;
            first = false;
            if batch.is_empty() {
                return Ok(());
            }
            // The same LRU head comes back on every query: stop rather than spin.
            if Self::unlink_batch(cache, batch, remove).evicted.is_empty() {
                crate::vprintln!("[CACHE]  Eviction stalled: no candidate could be removed");
                return Ok(());
            }
        }
    }

    /// The two steps after a claim: unlink the batch's files, with the lock free, then settle it,
    /// under the lock, and log what came of it, with the lock free again.
    fn unlink_batch(
        cache: &Mutex<AudioCache>,
        batch: Vec<Doomed>,
        remove: &dyn Fn(&Path) -> std::io::Result<()>,
    ) -> Settled {
        // A panic of the unlink must not leave the ids doomed for good: every file counts as
        // kept, its row goes back, and the panic carries on.
        let unlinked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            batch
                .iter()
                .map(|doomed| remove(&doomed.path))
                .collect::<Vec<_>>()
        }));
        let (results, panic) = match unlinked {
            Ok(results) => (results, None),
            Err(panic) => (
                batch
                    .iter()
                    .map(|_| Err(std::io::ErrorKind::Other.into()))
                    .collect(),
                Some(panic),
            ),
        };
        let settled = locked(cache).settle_unlinks(batch, results);
        for (id, size) in &settled.evicted {
            crate::vprintln!("[CACHE]  Evicted: {} (freed {} KB)", id, size / 1024);
        }
        for (id, e) in &settled.kept {
            crate::vprintln!("[CACHE]  Keeping the index row for {id}: {e}");
        }
        if let Some(panic) = panic {
            std::panic::resume_unwind(panic);
        }
        settled
    }

    /// Under the lock, the first step of a batch: choose at most [`EVICTION_BATCH`] LRU rows (for a
    /// trim, enough to reach the target; for a Clear, all) and take them out of the index in one
    /// transaction, ids `doomed` and bytes uncounted until [`settle_unlinks`](Self::settle_unlinks).
    /// Choosing and removing share one section: a chosen row was there, and from now on its id has
    /// no row, reservation or other batch, leaving its path untouched until the unlink.
    fn claim(&mut self, goal: Goal, cursor: &mut Cursor) -> Result<Vec<Doomed>, CacheError> {
        let target = self.eviction_target();
        let (keep, mut left) = match goal {
            Goal::Trim {
                keep,
                unreclaimable,
                first,
            } => {
                // A pass starts only over the cap and then drains to the target.
                if first && self.current_size <= self.max_bytes {
                    return Ok(Vec::new());
                }
                let left = self.current_size.saturating_sub(unreclaimable);
                if left <= target {
                    return Ok(Vec::new());
                }
                (keep, Some(left))
            }
            Goal::All => (None, None),
        };
        let Some(conn) = &self.conn else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            // `IS NOT`, not `!=`: a NULL `keep` excludes nothing.
            "SELECT track_id, format, file_size, created_at, last_access, access_count, gen
             FROM audio_cache WHERE track_id IS NOT ?2 AND (last_access, track_id) > (?3, ?4)
             ORDER BY last_access ASC, track_id ASC LIMIT ?1",
        )?;
        let candidates: Vec<Doomed> = stmt
            .query_map(
                params![EVICTION_BATCH, keep, cursor.last_access, cursor.id],
                |row| {
                    let id: String = row.get(0)?;
                    Ok(Doomed {
                        path: self.file_path(&id),
                        id,
                        format: row.get(1)?,
                        size: row.get(2)?,
                        created_at: row.get(3)?,
                        last_access: row.get(4)?,
                        access_count: row.get(5)?,
                        generation: row.get(6)?,
                    })
                },
            )?
            .filter_map(|r| r.ok())
            .collect();
        let mut doomed = Vec::new();
        for candidate in candidates {
            if let Some(left) = &mut left {
                if *left <= target {
                    break;
                }
                *left = left.saturating_sub(candidate.size as u64);
            }
            doomed.push(candidate);
        }
        let Some(last) = doomed.last() else {
            return Ok(Vec::new());
        };
        *cursor = Cursor {
            last_access: last.last_access,
            id: last.id.clone(),
        };
        let tx = conn.unchecked_transaction()?;
        for row in &doomed {
            tx.execute(
                "DELETE FROM audio_cache WHERE track_id = ?1",
                params![row.id],
            )?;
        }
        tx.commit()?;
        drop(stmt);
        for row in &doomed {
            self.doomed.insert(row.id.clone());
            self.current_size = self.current_size.saturating_sub(row.size as u64);
        }
        self.changed();
        Ok(doomed)
    }

    /// Under the lock, the last step of a batch: every id is given back. A file that went (or was
    /// already gone) is done; one that would not delete gets its row back as it was, its bytes
    /// counted again, for a later pass, a Clear or a store of the id to retry. Nothing can have
    /// removed that file in between: a doomed id is off limits to every store, drop and sweep.
    fn settle_unlinks(&mut self, batch: Vec<Doomed>, results: Vec<std::io::Result<()>>) -> Settled {
        let mut settled = Settled::default();
        let mut put_back = Vec::new();
        for (row, result) in batch.into_iter().zip(results) {
            self.doomed.remove(&row.id);
            match result {
                Ok(()) => settled.evicted.push((row.id, row.size)),
                // Already gone: the row was the orphan, and it is gone too.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    settled.evicted.push((row.id, row.size))
                }
                Err(e) => {
                    settled.kept.push((row.id.clone(), e));
                    put_back.push(row);
                }
            }
        }
        if !put_back.is_empty() {
            if let Err(e) = self.put_back(&put_back) {
                // On disk with no row, these bytes sit outside the cap and the size; `untrimmed`
                // tells the page until a Clear or the next start frees them.
                self.stranded += put_back.iter().map(|row| row.size as u64).sum::<u64>();
                crate::verr!(
                    "[CACHE]  {} undeleted file(s) left without a row: {e}",
                    put_back.len()
                );
            }
            self.changed();
        }
        settled
    }

    /// The rows of files that would not delete, as they were, and their bytes counted again.
    fn put_back(&mut self, rows: &[Doomed]) -> rusqlite::Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        let tx = conn.unchecked_transaction()?;
        for row in rows {
            tx.execute(
                "INSERT OR REPLACE INTO audio_cache
                 (track_id, format, file_size, created_at, last_access, access_count, gen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    row.id,
                    row.format,
                    row.size,
                    row.created_at,
                    row.last_access,
                    row.access_count,
                    row.generation
                ],
            )?;
        }
        tx.commit()?;
        for row in rows {
            self.current_size += row.size as u64;
        }
        Ok(())
    }
}

/// What a batch came to, for the log, written once the lock is free.
#[derive(Default)]
struct Settled {
    evicted: Vec<(String, i64)>,
    kept: Vec<(String, std::io::Error)>,
}

/// The event the page listens to, and the request it pulls with at mount.
pub(crate) const STATE_CHANNEL: &str = "settings.audio_cache_state";

/// Show the page a snapshot. Never under the cache lock. The page is reached through the app
/// state's lock, and no thread should hold one while it waits on the other.
pub(crate) fn publish(state: &CacheState) {
    crate::app_state::emit_ipc_event_with_data(STATE_CHANNEL, state);
}

/// One thread tells the page, whoever changed the cache: every change calls [`AudioCache::changed`]
/// under the lock, and the thread snapshots through `snapshot` (lock held only inside it) and hands
/// it to `publish` lock-free. The channel holds one token; changes pending share a snapshot.
pub(crate) fn spawn_publisher(
    snapshot: impl Fn() -> Option<CacheState> + Send + 'static,
    publish: impl Fn(&CacheState) + Send + 'static,
) -> std::io::Result<mpsc::SyncSender<()>> {
    let (tx, rx) = mpsc::sync_channel::<()>(1);
    // Not `thread::spawn`: a panic here would poison the lock for the process lifetime.
    std::thread::Builder::new()
        .name("cache-publisher".into())
        .spawn(move || {
            for () in rx {
                if let Some(state) = snapshot() {
                    publish(&state);
                }
            }
        })?;
    Ok(tx)
}

/// Watch the global cache, installed once its init has done its own writes; the boot seed has no
/// page to reach, and the page pulls at mount instead.
pub(crate) fn install_publisher(cache: &mut AudioCache) {
    let snapshot = || {
        let Ok(mut cache) = crate::state::AUDIO_CACHE.lock() else {
            crate::vprintln!("[CACHE]  Lock poisoned, state not published");
            return None;
        };
        Some(cache.state())
    };
    match spawn_publisher(snapshot, publish) {
        Ok(on_change) => cache.watch(on_change),
        Err(e) => crate::verr!("[CACHE]  No thread to publish the cache state: {e}"),
    }
}

type Run = Box<dyn FnOnce(&mut AudioCache) -> Result<(), CacheError> + Send>;

enum Change {
    Plain(Run),
    /// A Clear: the applier takes the move gate before the cache lock and runs the wipe with the
    /// lock free between steps, which a closure over `&mut AudioCache` could not do.
    Wipe,
    /// A new cap: the applier lowers it under the lock, then runs the eviction pass with the lock
    /// free between steps, which a closure over `&mut AudioCache` could not do.
    Resize(u64),
}

/// The reply a click waits on, owed once its change is applied: the state that change left. Dropped
/// unanswered by any path (a poisoned lock, no thread to apply it, a panic in the change), it fails
/// instead, as CEF keeps the query open until something answers and the page holds the click till then.
pub(crate) struct Completion(Option<Answer>);

type Answer = Box<dyn FnOnce(Result<CacheState, &'static str>) + Send>;

impl Completion {
    pub(crate) fn new(
        answer: impl FnOnce(Result<CacheState, &'static str>) + Send + 'static,
    ) -> Self {
        Self(Some(Box::new(answer)))
    }

    fn answer(mut self, state: CacheState) {
        if let Some(answer) = self.0.take() {
            answer(Ok(state));
        }
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(answer) = self.0.take() {
            answer(Err("audio cache change not applied"));
        }
    }
}

type Queued = (Request, Change, Option<Completion>);

/// Apply what `rx` delivers, in order, against `slot`, until every sender is gone.
fn apply_changes(rx: mpsc::Receiver<Queued>, slot: &Mutex<AudioCache>) {
    for (request, change, completion) in rx {
        match change {
            Change::Plain(run) => apply_one(slot, request, completion, run),
            Change::Wipe => {
                // Gate first, cache lock second: a move in flight is waited out here with the lock
                // free for every lookup, and no other lands until the Clear is answered; the reply
                // carries the state the wipe left, a download's row included only if it came after.
                let moves_off = hold_moves_off();
                if slot.is_poisoned() {
                    crate::vprintln!("[CACHE]  Lock poisoned, {request:?} skipped");
                    continue;
                }
                let outcome = AudioCache::wipe(slot, &moves_off, &|p| fs::remove_file(p));
                apply_one(slot, request, completion, move |cache| {
                    // What the sweep freed no row accounted for: a Clear that completed has
                    // taken the stranded bytes.
                    if outcome.is_ok() {
                        cache.stranded = 0;
                    }
                    outcome
                });
                drop(moves_off);
            }
            Change::Resize(max_bytes) => {
                if slot.is_poisoned() {
                    crate::vprintln!("[CACHE]  Lock poisoned, {request:?} skipped");
                    continue;
                }
                {
                    let mut cache = locked(slot);
                    cache.max_bytes = max_bytes;
                    cache.changed();
                }
                let outcome = AudioCache::drain(slot, None, 0, &|p| fs::remove_file(p));
                // The last step of the pass, the settling and the answer are one section: the
                // reply is numbered after every snapshot a step of the pass let the publisher
                // take, and carries the reason the pass left, if it did.
                apply_one(slot, request, completion, move |cache| {
                    cache.finish_pass(outcome)
                });
            }
        }
    }
}

fn apply_one(
    slot: &Mutex<AudioCache>,
    request: Request,
    completion: Option<Completion>,
    run: impl FnOnce(&mut AudioCache) -> Result<(), CacheError>,
) {
    let Ok(mut cache) = slot.lock() else {
        crate::vprintln!("[CACHE]  Lock poisoned, {request:?} skipped");
        return;
    };
    // Held across all three: the publisher can only snapshot the outcome with its record, and
    // the reply is numbered after every snapshot taken before the change.
    let outcome = run(&mut cache);
    cache.settle(request, outcome);
    let reply = completion.map(|completion| (completion, cache.state()));
    drop(cache);
    if let Some((completion, state)) = reply {
        completion.answer(state);
    }
}

/// One thread applies the user's changes, in the order they were made. Racing for the lock on
/// threads of their own, a switch flipped off then on could land off. And none of it can run on
/// the UI thread its handlers live on: a clear or a lowered cap deletes files.
static CHANGES: LazyLock<mpsc::Sender<Queued>> = LazyLock::new(|| {
    let (tx, rx) = mpsc::channel::<Queued>();
    let applier = move || apply_changes(rx, &crate::state::AUDIO_CACHE);
    // Not `thread::spawn`: a panic here would poison this lock for the process lifetime.
    if let Err(e) = std::thread::Builder::new()
        .name("cache-changes".into())
        .spawn(applier)
    {
        crate::verr!("[CACHE]  No thread to apply settings changes: {e}");
    }
    tx
});

/// Queue a change from the settings or the menu; the page hears the result once it is applied,
/// and the click that asked for it, if any, through `completion`.
pub(crate) fn queue_change(
    request: Request,
    change: impl FnOnce(&mut AudioCache) -> Result<(), CacheError> + Send + 'static,
    completion: Option<Completion>,
) {
    send_change(request, Change::Plain(Box::new(change)), completion);
}

/// Queue a Clear. Apart from [`queue_change`] because the wipe needs the move gate taken before
/// the cache lock, which only the applier can arrange.
pub(crate) fn queue_clear(completion: Option<Completion>) {
    send_change(Request::Clear, Change::Wipe, completion);
}

/// Queue a new cap. Apart from [`queue_change`] because the pass that lowers the cache to it must
/// not hold the cache lock while it unlinks.
pub(crate) fn queue_resize(max_bytes: u64, completion: Option<Completion>) {
    send_change(Request::Resize, Change::Resize(max_bytes), completion);
}

fn send_change(request: Request, change: Change, completion: Option<Completion>) {
    if CHANGES.send((request, change, completion)).is_err() {
        crate::verr!("[CACHE]  {request:?} not applied: no thread to apply it");
    }
}

#[cfg(test)]
#[path = "../../tests/unit/player/cache.rs"]
mod tests;
