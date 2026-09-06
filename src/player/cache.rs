use rusqlite::{Connection, params};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// What the cached files contain: v1 is TIDAL's ciphertext as fetched, v0 was
/// decrypted audio. Bump on any change to the contents - AES-CTR carries no auth
/// tag: a stale entry decodes to noise instead of failing. Wipe, never guess.
const CACHE_FORMAT_VERSION: i64 = 1;

/// Eviction hysteresis: evict until total_size < max_bytes * 0.9.
const EVICTION_FACTOR: f64 = 0.9;

/// Victims fetched per eviction query; a large overflow evicts in bounded
/// batches instead of materializing the whole table.
const EVICTION_BATCH: i64 = 64;

/// What a [`drop_entry`](AudioCache::drop_entry) attempt accomplished. A bare `bool` could
/// not separate "the file would not go and the row was kept" from "the file went, there was
/// no row"; those are opposite answers for a caller asking whether bytes remain on disk.
#[must_use = "a kept file leaves its row behind, which the caller has to account for"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropOutcome {
    /// File gone (removed, or already absent) and its index row with it.
    Dropped,
    /// File gone; there was no row to remove.
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
    /// Nothing indexed because the cache is disabled. No staging dir was ever handed out, and
    /// any file staged regardless is removed (with no index it could never be found again).
    Disabled,
    /// Nothing indexed because a clear raced this unlocked write;
    /// [`record_if_current`](AudioCache::record_if_current) removed the file it left behind.
    /// Distinct from [`Disabled`](Self::Disabled), letting the caller name the right cause.
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

pub struct AudioCache {
    /// `None`: disabled mode (no openable index location); lookups miss and
    /// stores are dropped.
    conn: Option<Connection>,
    audio_dir: PathBuf,
    max_bytes: u64,
    /// Bumped on every `clear()`. A store thread that began its unlocked
    /// `persist_file` before a clear must not re-insert its row afterwards.
    generation: u64,
    /// Running total of `file_size` across the index, maintained under the
    /// cache lock, and eviction never re-sums the table. Seeded from the DB at
    /// open; a crash just re-seeds next boot.
    current_size: u64,
}

impl AudioCache {
    /// Open (or create) the audio cache in the given data directory.
    pub fn open(data_dir: &Path) -> rusqlite::Result<Self> {
        Self::open_with_capacity(data_dir, DEFAULT_MAX_BYTES)
    }

    /// Open with a custom max capacity.
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
            CREATE INDEX IF NOT EXISTS idx_audio_cache_last_access
                ON audio_cache (last_access);",
        )?;

        Self::migrate_format(&conn, &audio_dir)?;
        Self::sweep_orphaned_staging(&audio_dir);

        // Seed the running size once; every mutation keeps it current thereafter.
        let current_size = conn
            .query_row(
                "SELECT COALESCE(SUM(file_size), 0) FROM audio_cache",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0) as u64;

        Ok(Self {
            conn: Some(conn),
            audio_dir,
            max_bytes,
            generation: 0,
            current_size,
        })
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
    pub fn disabled(data_dir: &Path) -> Self {
        Self {
            conn: None,
            audio_dir: data_dir.join("cache").join("audio"),
            max_bytes: DEFAULT_MAX_BYTES,
            generation: 0,
            current_size: 0,
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

    /// Check if a track exists in the index and return its file path.
    /// Does NOT read from disk - suitable for use under a short lock.
    pub fn lookup_path(&self, track_id: &str) -> Option<PathBuf> {
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

    /// Remove an orphaned index entry (file missing from disk). Reports whether a row
    /// went away; a caller cannot mistake a silent no-op for a deletion.
    pub fn remove_index_entry(&mut self, track_id: &str) -> bool {
        let Some(conn) = &self.conn else { return false };
        let prev_size: i64 = conn
            .query_row(
                "SELECT file_size FROM audio_cache WHERE track_id = ?1",
                params![track_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let deleted = conn
            .execute(
                "DELETE FROM audio_cache WHERE track_id = ?1",
                params![track_id],
            )
            .unwrap_or(0);
        if deleted > 0 {
            self.current_size = self.current_size.saturating_sub(prev_size as u64);
        }
        deleted > 0
    }

    /// Drop an entry: file first, index row only once the file is gone. A sharded file is
    /// reachable by accounting and eviction only through its row: losing the row while
    /// the file survives strands the bytes outside `max_bytes`.
    pub fn drop_entry(&mut self, track_id: &str) -> DropOutcome {
        let path = self.file_path(track_id);
        match fs::remove_file(&path) {
            Ok(()) => {}
            // Already gone: the row is the orphan.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                crate::vprintln!("[CACHE]  Keeping the index row for {track_id}: {e}");
                return DropOutcome::FileKept;
            }
        }
        if self.remove_index_entry(track_id) {
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

    /// Where a download stages its ciphertext, or `None` when the cache is disabled:
    /// with no index the file could never be looked up again; staging it would
    /// write and delete a full track for nothing. Staging inside the audio dir keeps
    /// [`persist_file`](Self::persist_file) a same-filesystem rename, and handing back
    /// the directory lets the caller create the file without the cache lock held.
    pub fn staging_dir(&self) -> Option<PathBuf> {
        self.conn.is_some().then(|| self.audio_dir.clone())
    }

    /// Move a completed staging file to its cache path (a rename, hence atomic). An
    /// associated function with no `&self` on purpose: it must run without the global
    /// cache lock: a concurrent lookup must not stall behind it. Pair with `record`.
    pub fn persist_file(path: &Path, staged: tempfile::NamedTempFile) -> anyhow::Result<()> {
        let shard_dir = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cache path has no parent: {}", path.display()))?;
        fs::create_dir_all(shard_dir)?;
        staged.persist(path)?;
        Ok(())
    }

    /// Take a completed ciphertext into the cache, off the calling thread.
    ///
    /// Three parts that have to stay together: resolve the path under a brief lock, write the
    /// multi-megabyte file UNLOCKED so a concurrent lookup does not stall behind it, then index
    /// and evict under a second short lock, discarding the write if a cache clear raced it.
    /// Written out twice, the second copy is where one of the three gets forgotten. The caller
    /// owes the checks this cannot make: that the download completed, and that no decoder
    /// rejected these bytes.
    pub fn store_ciphertext_detached(
        track_id: String,
        format: String,
        staged: tempfile::NamedTempFile,
        len: u64,
    ) {
        std::thread::spawn(move || {
            let (path, store_gen) = {
                let Ok(cache) = crate::state::AUDIO_CACHE.lock() else {
                    crate::vprintln!("[CACHE]  Lock poisoned, skipping store");
                    return;
                };
                (cache.file_path(&track_id), cache.generation())
            };
            if let Err(e) = Self::persist_file(&path, staged) {
                crate::vprintln!("[CACHE]  Store failed (persist): {e}");
                return;
            }
            let Ok(mut cache) = crate::state::AUDIO_CACHE.lock() else {
                crate::vprintln!("[CACHE]  Lock poisoned, skipping index");
                return;
            };
            match cache.record_if_current(&track_id, &format, len, store_gen) {
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
                // Nothing was staged; nothing to report.
                Ok(StoreOutcome::Disabled) => {}
                Ok(StoreOutcome::ClearedMidWrite) => crate::vprintln!(
                    "[CACHE]  Store discarded (cache cleared mid-write): {track_id}"
                ),
                Err(e) => crate::vprintln!("[CACHE]  Store failed (index): {e}"),
            }
        });
    }

    /// Retire an entry from a thread that must not wait for the cache lock.
    ///
    /// Both decode-failure sites run on the player control thread, which cannot afford this
    /// mutex: `clear()` holds it for its whole `remove_dir_all`, deliberately, and an inline
    /// drop parks the transport for as long as a wipe takes. Skipping is not open to them
    /// either, nothing else retiring an entry whose header sniffs fine and whose bitstream does
    /// not decode: every successful read touches `last_access`, so a failing entry keeps
    /// re-arming its own recency and eviction never reaches it. Late is correct here, never is
    /// not.
    ///
    /// Detached like a store, and nothing races it: a re-store of this id lands only on
    /// `DecodeEvent::Finished`, once a whole track has downloaded and decoded again, against a
    /// drop that takes microseconds.
    pub fn drop_entry_detached(track_id: String) {
        std::thread::spawn(move || {
            let Ok(mut cache) = crate::state::AUDIO_CACHE.lock() else {
                crate::vprintln!("[CACHE]  Lock poisoned, entry left indexed: {track_id}");
                return;
            };
            if cache.drop_entry(&track_id) == DropOutcome::Dropped {
                crate::vprintln!("[CACHE]  Dropped after a decode failure: {track_id}");
            }
        });
    }

    /// How far an overflow eviction drains, never a per-entry size limit. Eviction starts
    /// only once `current_size` passes `max_bytes`; this is just the hysteresis floor.
    fn eviction_target(&self) -> u64 {
        (self.max_bytes as f64 * EVICTION_FACTOR) as u64
    }

    /// Record an already-written track in the index and evict LRU entries if
    /// over capacity. Short critical section: index insert + eviction only, no
    /// large I/O. Pair with [`persist_file`](Self::persist_file).
    pub fn record(
        &mut self,
        track_id: &str,
        format: &str,
        file_size: u64,
    ) -> anyhow::Result<StoreOutcome> {
        if self.conn.is_none() {
            return Ok(StoreOutcome::Disabled);
        }
        // Admission control, before the row exists. The bound is `max_bytes`, not the
        // eviction target: eviction only starts above `max_bytes`, and the target merely says
        // how far a pass that started should drain. The one size no pass can resolve is an
        // entry bigger than the whole cache, holding `current_size` over the cap with nothing
        // else left to evict.
        //
        // `drop_entry`, not a bare `remove_file`: a row for this id can already exist, an
        // entry that failed to decrypt being kept on purpose (`CacheReadError::Unreadable`).
        let mut unremovable = false;
        if file_size > self.max_bytes {
            match self.drop_entry(track_id) {
                // Ungated (a track that will never cache has no other channel), and the one
                // site holding both sizes.
                DropOutcome::Dropped | DropOutcome::NoRow => {
                    crate::verr!(
                        "[CACHE]  Not cached, {} exceeds the {} cache: {}",
                        crate::player::format_bytes(file_size),
                        crate::player::format_bytes(self.max_bytes),
                        track_id
                    );
                    return Ok(StoreOutcome::TooLarge);
                }
                // The bytes are there and will not go. Indexing them at their real size is
                // the only honest option; unindexed they escape accounting and eviction both.
                DropOutcome::FileKept => unremovable = true,
            }
        }
        // Unreachable, the disabled case having returned above; kept because `conn`'s
        // immutable borrow must follow `drop_entry`'s `&mut self`.
        let Some(conn) = &self.conn else {
            return Ok(StoreOutcome::Disabled);
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
            "INSERT OR REPLACE INTO audio_cache (track_id, format, file_size, created_at, last_access, access_count)
             VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            params![track_id, format, file_size as i64, now, stamp],
        )?;
        self.current_size = (self.current_size + file_size).saturating_sub(prev_size as u64);

        // An undeletable oversized row is the one thing eviction can never free. Every other
        // store hands over zero, keeping its own bytes in the measurement.
        self.evict_if_needed(track_id, if unremovable { file_size } else { 0 })?;

        Ok(if unremovable {
            StoreOutcome::TooLargeRetained
        } else {
            StoreOutcome::Kept
        })
    }

    /// Current clear-generation. Snapshot this before an unlocked
    /// [`persist_file`](Self::persist_file) and pass it to
    /// [`record_if_current`](Self::record_if_current).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Like [`record`](Self::record), but a no-op if the cache was cleared
    /// (generation changed) since `expected_gen` was snapshotted. In that case
    /// the just-written file is removed, and a clear cannot leave an orphan.
    pub fn record_if_current(
        &mut self,
        track_id: &str,
        format: &str,
        file_size: u64,
        expected_gen: u64,
    ) -> anyhow::Result<StoreOutcome> {
        if self.conn.is_none() {
            let _ = fs::remove_file(self.file_path(track_id));
            return Ok(StoreOutcome::Disabled);
        }
        if self.generation != expected_gen {
            let _ = fs::remove_file(self.file_path(track_id));
            return Ok(StoreOutcome::ClearedMidWrite);
        }
        self.record(track_id, format, file_size)
    }

    pub fn clear(&mut self) -> anyhow::Result<()> {
        let (count, total): (i64, u64) = match &self.conn {
            Some(conn) => (
                conn.query_row("SELECT COUNT(*) FROM audio_cache", [], |row| row.get(0))
                    .unwrap_or(0),
                self.total_size(),
            ),
            None => (0, 0),
        };

        // Fail closed: the rows are the only handle on these files, and dropping them
        // after a failed wipe leaves the survivors outside the size cap for good.
        if self.audio_dir.exists()
            && let Err(e) = fs::remove_dir_all(&self.audio_dir)
        {
            anyhow::bail!("could not clear the audio cache directory: {e}");
        }
        fs::create_dir_all(&self.audio_dir).ok();

        if let Some(conn) = &self.conn {
            conn.execute("DELETE FROM audio_cache", [])?;
        }
        // Invalidate any store that began its unlocked write before this clear.
        self.generation = self.generation.wrapping_add(1);
        self.current_size = 0;

        crate::vprintln!(
            "[CACHE]  Cleared {} entries ({:.1} MB)",
            count,
            total as f64 / (1024.0 * 1024.0)
        );
        Ok(())
    }

    /// The running total of cached bytes (maintained incrementally, not re-summed).
    pub fn total_size(&self) -> u64 {
        self.current_size
    }

    /// Evict LRU entries until the running size is under `max_bytes * EVICTION_FACTOR`, oldest
    /// first via the `last_access` index (no full sort), in bounded batches.
    ///
    /// `keep` is the row [`record`](Self::record) just inserted, never a candidate here; no
    /// store is undone by the eviction it triggered. `unreclaimable` is how many of its bytes
    /// no pass could ever free; counted into the total, they hold it above a mark no candidate
    /// can reach and the pass empties the cache trying. Only
    /// [`TooLargeRetained`](StoreOutcome::TooLargeRetained) sets it (an oversized file that
    /// would not delete); an ordinary store passes zero, its row being out of scope for this
    /// pass yet evictable by the next.
    fn evict_if_needed(&mut self, keep: &str, unreclaimable: u64) -> anyhow::Result<()> {
        if self.current_size <= self.max_bytes {
            return Ok(());
        }
        let target = self.eviction_target();

        while self.current_size.saturating_sub(unreclaimable) > target {
            let batch: Vec<(String, i64)> = {
                let Some(conn) = &self.conn else {
                    return Ok(());
                };
                let mut stmt = conn.prepare(
                    "SELECT track_id, file_size FROM audio_cache
                     WHERE track_id != ?2 ORDER BY last_access ASC LIMIT ?1",
                )?;
                stmt.query_map(params![EVICTION_BATCH, keep], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .filter_map(|r| r.ok())
                .collect()
            };
            if batch.is_empty() {
                break;
            }
            let mut evicted_any = false;
            for (evict_id, size) in batch {
                if self.current_size.saturating_sub(unreclaimable) <= target {
                    break;
                }
                // Only a real drop frees bytes. A kept file leaves its row on purpose, and a
                // missing row freed nothing; neither advances this pass.
                if self.drop_entry(&evict_id) != DropOutcome::Dropped {
                    continue;
                }
                evicted_any = true;
                crate::vprintln!("[CACHE]  Evicted: {} (freed {} KB)", evict_id, size / 1024);
            }
            // The same LRU head comes back on every query: stop rather than spin.
            if !evicted_any {
                crate::vprintln!("[CACHE]  Eviction stalled: no candidate could be removed");
                break;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/player/cache.rs"]
mod tests;
