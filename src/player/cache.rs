use rusqlite::{Connection, params};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Eviction hysteresis: evict until total_size < max_bytes * 0.9.
const EVICTION_FACTOR: f64 = 0.9;

/// Victims fetched per eviction query, so a large overflow evicts in bounded
/// batches instead of materializing the whole table.
const EVICTION_BATCH: i64 = 64;

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Monotonic stamp for the `last_access` LRU sort key: strictly greater than any
/// existing value, so a backward wall-clock step can't make a fresh entry sort
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
    /// `write_file` before a clear must not re-insert its row afterwards.
    generation: u64,
    /// Running total of `file_size` across the index, maintained under the
    /// cache lock so eviction never re-sums the table. Seeded from the DB at
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

    /// On-disk path for a track. Depends only on the (fixed) audio dir, so the
    /// caller can compute it under a brief lock and then write the file without
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
    /// went away, so a caller cannot mistake a silent no-op for a deletion.
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

    /// Drop an entry, reporting whether it went away: file first, index row only once
    /// the file is gone. A sharded file is reachable by accounting and eviction only
    /// through its row, so losing the row while the file survives strands the bytes
    /// outside `max_bytes`.
    pub fn drop_entry(&mut self, track_id: &str) -> bool {
        let path = self.file_path(track_id);
        match fs::remove_file(&path) {
            Ok(()) => {}
            // Already gone: the row is the orphan.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                crate::vprintln!("[CACHE]  Keeping the index row for {track_id}: {e}");
                return false;
            }
        }
        self.remove_index_entry(track_id)
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

    /// Atomically write a fully-downloaded track to its cache path (temp file
    /// in the same directory, then rename). Intentionally an associated
    /// function with no `&self`: this is the multi-MB I/O and must run WITHOUT
    /// holding the global cache lock, so a concurrent track-load lookup is not
    /// stalled behind the write. Pair with [`record`](Self::record).
    pub fn write_file(path: &Path, data: &[u8]) -> anyhow::Result<()> {
        let shard_dir = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cache path has no parent: {}", path.display()))?;
        fs::create_dir_all(shard_dir)?;

        let tmp = tempfile::NamedTempFile::new_in(shard_dir)?;
        fs::write(tmp.path(), data)?;
        tmp.persist(path)?;
        Ok(())
    }

    /// Record an already-written track in the index and evict LRU entries if
    /// over capacity. Short critical section: index insert + eviction only, no
    /// large I/O. Pair with [`write_file`](Self::write_file).
    pub fn record(&mut self, track_id: &str, format: &str, file_size: u64) -> anyhow::Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        let now = now_epoch();
        let stamp = next_access_stamp(conn, now);
        // INSERT OR REPLACE overwrites an existing row, so the running total moves
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

        self.evict_if_needed()?;

        Ok(())
    }

    /// Current clear-generation. Snapshot this before an unlocked
    /// [`write_file`](Self::write_file) and pass it to
    /// [`record_if_current`](Self::record_if_current).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Like [`record`](Self::record), but a no-op if the cache was cleared
    /// (generation changed) since `expected_gen` was snapshotted. In that case
    /// the just-written file is removed so a clear can't leave an orphan.
    /// Returns `Ok(true)` if recorded, `Ok(false)` if skipped due to a clear
    /// or a disabled cache.
    pub fn record_if_current(
        &mut self,
        track_id: &str,
        format: &str,
        file_size: u64,
        expected_gen: u64,
    ) -> anyhow::Result<bool> {
        if self.conn.is_none() || self.generation != expected_gen {
            let _ = fs::remove_file(self.file_path(track_id));
            return Ok(false);
        }
        self.record(track_id, format, file_size)?;
        Ok(true)
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

        // Fail closed: the rows are the only handle on these files, so dropping them
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

    /// Evict LRU entries until the running size is under `max_bytes * EVICTION_FACTOR`,
    /// oldest first via the `last_access` index (no full sort), in bounded batches.
    fn evict_if_needed(&mut self) -> anyhow::Result<()> {
        if self.current_size <= self.max_bytes {
            return Ok(());
        }
        let target = (self.max_bytes as f64 * EVICTION_FACTOR) as u64;

        while self.current_size > target {
            let batch: Vec<(String, i64)> = {
                let Some(conn) = &self.conn else {
                    return Ok(());
                };
                let mut stmt = conn.prepare(
                    "SELECT track_id, file_size FROM audio_cache ORDER BY last_access ASC LIMIT ?1",
                )?;
                stmt.query_map(params![EVICTION_BATCH], |row| {
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
                if self.current_size <= target {
                    break;
                }
                // Keeps the row when the file will not go, so the bytes stay accounted
                // for instead of becoming an invisible orphan.
                if !self.drop_entry(&evict_id) {
                    continue;
                }
                evicted_any = true;
                crate::vprintln!("[CACHE]  Evicted: {} (freed {} KB)", evict_id, size / 1024);
            }
            // The same LRU head comes back on every query, so stop rather than spin.
            if !evicted_any {
                crate::vprintln!("[CACHE]  Eviction stalled: no candidate could be removed");
                break;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE audio_cache (
                track_id     TEXT PRIMARY KEY,
                format       TEXT NOT NULL,
                file_size    INTEGER NOT NULL,
                created_at   INTEGER NOT NULL,
                last_access  INTEGER NOT NULL,
                access_count INTEGER DEFAULT 1
            );
            CREATE INDEX IF NOT EXISTS idx_audio_cache_last_access
                ON audio_cache (last_access);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn access_stamp_is_monotonic_across_clock_skew() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO audio_cache (track_id, format, file_size, created_at, last_access, access_count)
             VALUES ('a', 'flac', 1, 1000, 1000, 1)",
            [],
        )
        .unwrap();
        // Clock rewound to 50: the stamp must still exceed the existing max so a
        // fresh entry can't sort older than entries it was accessed after.
        assert_eq!(next_access_stamp(&conn, 50), 1001);
        // A normal forward clock is honored as-is.
        assert_eq!(next_access_stamp(&conn, 5000), 5000);
    }

    #[test]
    fn access_stamp_on_empty_table_uses_now() {
        let conn = test_conn();
        assert_eq!(next_access_stamp(&conn, 100), 100);
    }

    #[test]
    fn disabled_cache_misses_and_noops() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cache = AudioCache::disabled(tmp.path());
        assert_eq!(cache.lookup_path("track"), None);
        assert_eq!(cache.total_size(), 0);
        cache.touch("track");
        cache.remove_index_entry("track");
        assert!(cache.record("track", "flac", 10).is_ok());
        assert_eq!(cache.lookup_path("track"), None);
        assert!(cache.clear().is_ok());
    }

    #[test]
    fn disabled_record_if_current_drops_the_written_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cache = AudioCache::disabled(tmp.path());
        let path = cache.file_path("track");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"pcm").unwrap();
        let cur_gen = cache.generation();
        assert!(
            !cache
                .record_if_current("track", "flac", 3, cur_gen)
                .unwrap()
        );
        assert!(!path.exists());
    }

    #[test]
    fn dropping_an_entry_clears_the_file_the_row_and_the_accounting() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cache = AudioCache::open_with_capacity(tmp.path(), 1000).unwrap();
        let path = cache.file_path("track");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"ciphertext").unwrap();
        cache.record("track", "flac", 10).unwrap();

        assert!(cache.drop_entry("track"));
        assert!(!path.exists());
        assert_eq!(cache.lookup_path("track"), None);
        assert_eq!(cache.total_size(), 0);
    }

    /// The row is itself the orphan here, so dropping has to succeed rather than
    /// refuse for want of a file.
    #[test]
    fn dropping_an_entry_whose_file_already_vanished_still_clears_the_row() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cache = AudioCache::open_with_capacity(tmp.path(), 1000).unwrap();
        cache.record("track", "flac", 10).unwrap();

        assert!(cache.drop_entry("track"));
        assert_eq!(cache.lookup_path("track"), None);
        assert_eq!(cache.total_size(), 0);
    }

    #[test]
    fn running_size_tracks_inserts_evictions_and_clear() {
        let tmp = tempfile::tempdir().unwrap();
        // Small cap so a few inserts cross it and trigger eviction (target = 90).
        let mut cache = AudioCache::open_with_capacity(tmp.path(), 100).unwrap();
        assert_eq!(cache.total_size(), 0);

        cache.record("a", "flac", 30).unwrap();
        cache.record("b", "flac", 40).unwrap();
        assert_eq!(cache.total_size(), 70);

        // Re-recording an existing id replaces it: the total moves by the delta.
        cache.record("a", "flac", 50).unwrap();
        assert_eq!(cache.total_size(), 90);

        // Crossing the cap evicts the oldest (b) down to the 90% target.
        cache.record("c", "flac", 40).unwrap();
        assert_eq!(cache.total_size(), 90);
        assert!(cache.lookup_path("b").is_none());

        cache.clear().unwrap();
        assert_eq!(cache.total_size(), 0);
    }
}
