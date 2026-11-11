use rusqlite::{Connection, params};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

mod checkpoint;
mod evict;
mod open;
mod queue;
mod store;
use checkpoint::Checkpointer;
pub(crate) use queue::{Completion, queue_change, queue_clear, queue_resize};

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
    /// The thread that checkpoints the index's log, off the lock; `None` where SQLite's own
    /// automatic checkpoint is still on.
    checkpointer: Option<Checkpointer>,
}

impl Drop for AudioCache {
    /// The checkpoint thread ends before the connection it was started beside closes.
    fn drop(&mut self) {
        if let Some(mut checkpointer) = self.checkpointer.take() {
            checkpointer.stop(self.conn.as_ref());
        }
    }
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
    ///
    /// [`Reservation::move_in`]: store::Reservation::move_in
    pub fn staging_dir(&self) -> Option<PathBuf> {
        (self.conn.is_some() && self.enabled).then(|| self.audio_dir.clone())
    }
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

#[cfg(test)]
#[path = "../../../tests/unit/player/cache.rs"]
mod tests;

#[cfg(test)]
#[path = "../../../tests/unit/player/cache/fixture.rs"]
mod fixture;
