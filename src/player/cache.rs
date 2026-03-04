use rusqlite::{Connection, params};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default maximum cache size: 2 GB.
const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Eviction hysteresis: evict until total_size < max_bytes * 0.9.
const EVICTION_FACTOR: f64 = 0.9;

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
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

/// 2-char hex shard prefix from the hash.
fn shard_prefix(hash: &str) -> &str {
    &hash[..2]
}

pub struct AudioCache {
    conn: Connection,
    audio_dir: PathBuf,
    max_bytes: u64,
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

        // Ensure directories exist
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
        })
    }

    /// Path to the cached audio file for a given track.
    fn file_path(&self, track_id: &str) -> PathBuf {
        let hash = track_hash(track_id);
        let shard = shard_prefix(&hash);
        self.audio_dir.join(shard).join(&hash)
    }

    /// Load a cached track's audio data into RAM. Returns None on miss.
    pub fn load(&self, track_id: &str) -> Option<Vec<u8>> {
        let path = self.file_path(track_id);
        let data = fs::read(&path).ok()?;

        // Verify entry exists in index
        let exists: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM audio_cache WHERE track_id = ?1",
                params![track_id],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !exists {
            // Orphaned file — clean up
            let _ = fs::remove_file(&path);
            return None;
        }

        // Update access metadata
        let now = now_epoch();
        let _ = self.conn.execute(
            "UPDATE audio_cache SET last_access = ?1, access_count = access_count + 1 WHERE track_id = ?2",
            params![now, track_id],
        );

        Some(data)
    }

    /// Store a fully-downloaded track in the cache.
    /// Atomic: writes to a temp file then renames.
    /// Evicts LRU entries if over capacity.
    pub fn store(&mut self, track_id: &str, format: &str, data: &[u8]) -> anyhow::Result<()> {
        let path = self.file_path(track_id);
        let shard_dir = path.parent().unwrap();
        fs::create_dir_all(shard_dir)?;

        // Atomic write via tempfile in same directory (same filesystem for rename)
        let tmp = tempfile::NamedTempFile::new_in(shard_dir)?;
        fs::write(tmp.path(), data)?;
        tmp.persist(&path)?;

        let now = now_epoch();
        let file_size = data.len() as i64;

        self.conn.execute(
            "INSERT OR REPLACE INTO audio_cache (track_id, format, file_size, created_at, last_access, access_count)
             VALUES (?1, ?2, ?3, ?4, ?4, 1)",
            params![track_id, format, file_size, now],
        )?;

        // Evict if over capacity
        self.evict_if_needed()?;

        Ok(())
    }

    /// Total size of all cached files according to the index.
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
            let _ = fs::remove_file(&path);
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
