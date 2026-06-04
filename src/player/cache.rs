use rusqlite::{Connection, params};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Eviction hysteresis: evict until total_size < max_bytes * 0.9.
const EVICTION_FACTOR: f64 = 0.9;

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
    conn: Connection,
    audio_dir: PathBuf,
    max_bytes: u64,
    /// Bumped on every `clear()`. A store thread that began its unlocked
    /// `write_file` before a clear must not re-insert its row afterwards.
    generation: u64,
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
            );",
        )?;

        Ok(Self {
            conn,
            audio_dir,
            max_bytes,
            generation: 0,
        })
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
        let exists: bool = self
            .conn
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

    /// Remove an orphaned index entry (file missing from disk).
    pub fn remove_index_entry(&self, track_id: &str) {
        let _ = self.conn.execute(
            "DELETE FROM audio_cache WHERE track_id = ?1",
            params![track_id],
        );
    }

    /// Update access metadata after a successful cache read.
    pub fn touch(&self, track_id: &str) {
        let stamp = next_access_stamp(&self.conn, now_epoch());
        let _ = self.conn.execute(
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
        let now = now_epoch();
        let stamp = next_access_stamp(&self.conn, now);
        self.conn.execute(
            "INSERT OR REPLACE INTO audio_cache (track_id, format, file_size, created_at, last_access, access_count)
             VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            params![track_id, format, file_size as i64, now, stamp],
        )?;

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
    /// Returns `Ok(true)` if recorded, `Ok(false)` if skipped due to a clear.
    pub fn record_if_current(
        &mut self,
        track_id: &str,
        format: &str,
        file_size: u64,
        expected_gen: u64,
    ) -> anyhow::Result<bool> {
        if self.generation != expected_gen {
            let _ = fs::remove_file(self.file_path(track_id));
            return Ok(false);
        }
        self.record(track_id, format, file_size)?;
        Ok(true)
    }

    pub fn clear(&mut self) -> anyhow::Result<()> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM audio_cache", [], |row| row.get(0))
            .unwrap_or(0);
        let total = self.total_size();

        if self.audio_dir.exists() {
            fs::remove_dir_all(&self.audio_dir).ok();
            fs::create_dir_all(&self.audio_dir).ok();
        }

        self.conn.execute("DELETE FROM audio_cache", [])?;
        // Invalidate any store that began its unlocked write before this clear.
        self.generation = self.generation.wrapping_add(1);

        crate::vprintln!(
            "[CACHE]  Cleared {} entries ({:.1} MB)",
            count,
            total as f64 / (1024.0 * 1024.0)
        );
        Ok(())
    }

    pub fn total_size(&self) -> u64 {
        self.conn
            .query_row(
                "SELECT COALESCE(SUM(file_size), 0) FROM audio_cache",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0) as u64
    }

    /// Evict LRU entries until total_size < max_bytes * EVICTION_FACTOR.
    fn evict_if_needed(&mut self) -> anyhow::Result<()> {
        let total = self.total_size();
        if total <= self.max_bytes {
            return Ok(());
        }

        let target = (self.max_bytes as f64 * EVICTION_FACTOR) as u64;
        let mut current = total;

        let mut stmt = self
            .conn
            .prepare("SELECT track_id, file_size FROM audio_cache ORDER BY last_access ASC")?;

        let entries: Vec<(String, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();

        for (evict_id, size) in entries {
            if current <= target {
                break;
            }

            let path = self.file_path(&evict_id);
            if let Err(e) = fs::remove_file(&path) {
                crate::vprintln!("[CACHE]  Failed to evict {}: {e}", evict_id);
            }
            self.conn.execute(
                "DELETE FROM audio_cache WHERE track_id = ?1",
                params![evict_id],
            )?;
            current = current.saturating_sub(size as u64);

            crate::vprintln!("[CACHE]  Evicted: {} (freed {} KB)", evict_id, size / 1024);
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
            );",
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
}
