//! The transcoded covers, on disk, bounded by their total size.
//!
//! Not the audio cache: that one indexes tracks in SQLite and moves files through claims and
//! settles, none of which a few megabytes of WebM per cover needs. One file per cover, named by
//! the hash of its key and format, written whole and renamed into place, evicted oldest-used first.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::video::cover::CoverKey;

/// Room for a few dozen covers at 1280, more at 640.
const BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// Bumped whenever what writes a cover changes its bytes: the page's encoder settings, its muxer,
/// the frames Rust hands it. Hashed into each file's name. A cover an older format wrote is never
/// served again and, used no more, is the first one evicted.
const COVER_FORMAT: u32 = 2;

/// A file that is not a cover and has not been touched for this long is a temporary one a crash
/// left behind. Younger ones may be another store's, still being written.
pub(crate) const STALE_AFTER: Duration = Duration::from_secs(600);

pub(crate) struct CoverCache {
    dir: PathBuf,
    budget: u64,
}

pub(crate) static COVER_CACHE: std::sync::LazyLock<CoverCache> = std::sync::LazyLock::new(|| {
    CoverCache::new(
        crate::state::cache_data_dir().join("cache").join("covers"),
        BUDGET_BYTES,
    )
});

/// One file in the cache directory, as the eviction rule sees it.
#[derive(Debug, Clone)]
pub(crate) struct CacheEntry {
    pub(crate) path: PathBuf,
    pub(crate) len: u64,
    pub(crate) used: SystemTime,
}

/// A cover file's name: 64 lowercase hex digits and `.webm`.
fn is_cover_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".webm"))
        .is_some_and(|hash| {
            hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
}

/// Which files to delete: stale leftovers first, then covers oldest-used first until the rest
/// fits `budget`.
pub(crate) fn plan_eviction(
    mut entries: Vec<CacheEntry>,
    budget: u64,
    now: SystemTime,
) -> Vec<PathBuf> {
    let mut doomed = Vec::new();
    entries.retain(|entry| {
        if is_cover_file(&entry.path) {
            return true;
        }
        let stale = now
            .duration_since(entry.used)
            .is_ok_and(|age| age >= STALE_AFTER);
        if stale {
            doomed.push(entry.path.clone());
        }
        false
    });
    entries.sort_by_key(|entry| entry.used);
    let mut total: u64 = entries.iter().map(|entry| entry.len).sum();
    for entry in entries {
        if total <= budget {
            break;
        }
        total -= entry.len;
        doomed.push(entry.path);
    }
    doomed
}

impl CoverCache {
    pub(crate) fn new(dir: PathBuf, budget: u64) -> Self {
        Self { dir, budget }
    }

    fn path_of(&self, key: &CoverKey) -> PathBuf {
        let named = format!("{COVER_FORMAT}\0{}", key.url());
        let hash = crate::video::codec::sha256_bytes(named.as_bytes());
        self.dir.join(format!("{hash}.webm"))
    }

    /// The cover's bytes, its file marked as just used. `None` when it is not cached.
    pub(crate) fn get(&self, key: &CoverKey) -> Option<Vec<u8>> {
        let path = self.path_of(key);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                crate::verr!("[VIDEO]   Reading a cached cover failed: {e}");
                return None;
            }
        };
        // The modification time is the recency eviction reads: no index to keep in step.
        let touched = std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(SystemTime::now()));
        if let Err(e) = touched {
            crate::vprintln!("[VIDEO]   Marking a cached cover used failed: {e}");
        }
        Some(bytes)
    }

    /// Write the cover whole under a temporary name, rename it into place, then evict. A reader
    /// finds the old file or the new one, never a part of one.
    pub(crate) fn put(&self, key: &CoverKey, bytes: &[u8]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let mut staged = tempfile::NamedTempFile::new_in(&self.dir)?;
        staged.write_all(bytes)?;
        staged.as_file().sync_all()?;
        // A lost entry after a power cut is a cover transcoded again: no directory flush.
        crate::video::codec::persist_outlasting_holds(staged, &self.path_of(key))?;
        self.evict();
        Ok(())
    }

    /// Delete every cover: an eviction to an empty budget. A store still being written lands its
    /// cover after the Clear. A cover on screen keeps playing what its element loaded.
    pub(crate) fn clear(&self) {
        self.sweep(0);
    }

    fn evict(&self) {
        self.sweep(self.budget);
    }

    fn sweep(&self, budget: u64) {
        let Ok(listing) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let entries = listing
            .flatten()
            .filter_map(|entry| {
                let meta = entry.metadata().ok().filter(|m| m.is_file())?;
                Some(CacheEntry {
                    path: entry.path(),
                    len: meta.len(),
                    used: meta.modified().ok()?,
                })
            })
            .collect();
        for path in plan_eviction(entries, budget, SystemTime::now()) {
            if let Err(e) = std::fs::remove_file(&path) {
                crate::vprintln!("[VIDEO]   Deleting a cached cover failed: {e}");
            }
        }
    }
}

/// The user's "Clear cache", beside the audio cache's: off the UI thread it is called from.
pub(crate) fn queue_clear() {
    crate::state::rt_handle().spawn_blocking(|| COVER_CACHE.clear());
}

#[cfg(test)]
#[path = "../../tests/unit/video/cover_cache.rs"]
mod tests;
