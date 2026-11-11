use rusqlite::params;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::store::{MovesHeldOff, Wiping};
use super::{AudioCache, CacheError, EVICTION_BATCH, HELD, OWNED_MAX, row_names, track_hash};

/// The eviction pass a store's row owes, to run once the lock is let go.
pub(super) struct Owed {
    pub(super) keep: String,
    pub(super) unreclaimable: u64,
}

/// A row an eviction took out of the index, with everything needed to put it back should its file
/// refuse to go.
pub(super) struct Doomed {
    pub(super) id: String,
    format: String,
    size: i64,
    created_at: i64,
    last_access: i64,
    access_count: i64,
    generation: i64,
    pub(super) path: PathBuf,
}

/// One lock section of a pass over the shared cache. The lock is taken per step and is free while
/// the files are unlinked.
pub(super) fn locked(cache: &Mutex<AudioCache>) -> std::sync::MutexGuard<'_, AudioCache> {
    cache.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where a pass stands in the LRU order, `(last_access, track_id)`: a total order, so that rows
/// tied on a stamp (a `put_back` restoring one a later insert has reused) are neither skipped
/// nor met twice across two batches.
pub(super) struct Cursor {
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
pub(super) enum Goal<'a> {
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

impl AudioCache {
    /// A Clear, `MOVE_GATE` held for writing (the proof): its rows go through an eviction's steps
    /// (claim, unlink lock-free, settle), then what no row describes is swept out of `audio/`, lock-free
    /// too. No folder is renamed or removed wholesale: a download's open file cannot fail the Clear.
    ///
    /// A file that will not delete puts its row back (counted, fail closed) and the Clear ends in
    /// its first refusal after freeing the rest; a second Clear retries only what is left. Staging
    /// files at the root of `audio/` stay: a download in flight moves in after the Clear, at the gate.
    pub(super) fn wipe(
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
    pub(super) fn finish_pass(
        &mut self,
        outcome: Result<(), CacheError>,
    ) -> Result<(), CacheError> {
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
    /// reach and the pass empties the cache trying. Only [`TooLargeRetained`](super::StoreOutcome::TooLargeRetained)
    /// sets it (an oversized file that would not delete); an ordinary store passes zero, its row
    /// being out of scope for this pass yet evictable by the next. A pass no store triggered (a
    /// lowered cap) has neither.
    pub(super) fn drain(
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
    pub(super) fn claim(
        &mut self,
        goal: Goal,
        cursor: &mut Cursor,
    ) -> Result<Vec<Doomed>, CacheError> {
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
    pub(super) fn settle_unlinks(
        &mut self,
        batch: Vec<Doomed>,
        results: Vec<std::io::Result<()>>,
    ) -> Settled {
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
pub(super) struct Settled {
    evicted: Vec<(String, i64)>,
    kept: Vec<(String, std::io::Error)>,
}

#[cfg(test)]
#[path = "../../../tests/unit/player/cache/evict.rs"]
mod tests;
