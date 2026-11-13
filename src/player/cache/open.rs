use rusqlite::Connection;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

use super::checkpoint::Checkpointer;
use super::evict::locked;
use super::{AudioCache, CACHE_FORMAT_VERSION, row_names};

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

        // Last: SQLite's own checkpoint serves the writes above, and goes off only once the
        // thread that replaces it runs.
        let checkpointer = Checkpointer::start(&conn, &db_path);

        // No eviction here: a cache over its cap (the cap lowered by a session that ended before
        // its own pass did) is trimmed by [`settle_after_open`] once the cache is shared. The init
        // the lock waits behind never unlinks.
        Ok(Self {
            checkpointer,
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
            closed: false,
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

    pub(super) fn take_generation(&mut self) -> i64 {
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
            checkpointer: None,
            closed: false,
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/player/cache/open.rs"]
mod tests;
