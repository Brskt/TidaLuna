use rusqlite::params;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::evict::{Owed, locked};
use super::{
    AudioCache, DropOutcome, EVICTION_FACTOR, Generation, StoreOutcome, next_access_stamp,
    now_epoch,
};

/// The right to move one download onto its cache path, held from [`AudioCache::reserve`] to
/// [`AudioCache::commit_row`]. While it stands nothing else stores that id.
#[derive(Debug)]
pub struct Reservation {
    track_id: String,
    pub(super) path: PathBuf,
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
pub(super) struct Wiping<'a>(&'a Mutex<AudioCache>);

impl<'a> Wiping<'a> {
    pub(super) fn begin(cache: &'a Mutex<AudioCache>) -> Self {
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
pub(super) struct MovesHeldOff(
    #[expect(dead_code, reason = "held for its Drop, which releases the gate")]
    std::sync::RwLockWriteGuard<'static, ()>,
);

/// Taken by `apply_changes` ahead of the cache lock, given back once the Clear is answered;
/// first waits for the moves in flight.
pub(super) fn hold_moves_off() -> MovesHeldOff {
    MovesHeldOff(MOVE_GATE.write().unwrap_or_else(|e| e.into_inner()))
}

impl Reservation {
    /// Takes no cache lock, only the gate a Clear holds for its wipe.
    pub fn move_in(&self, staged: tempfile::NamedTempFile) -> anyhow::Result<()> {
        let _moving = MOVE_GATE.read().unwrap_or_else(|e| e.into_inner());
        AudioCache::persist_file(&self.path, staged)
    }
}

impl AudioCache {
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
    pub(super) fn commit_row(
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
    pub(super) fn drop_after_decode_failure(
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
    pub(super) fn eviction_target(&self) -> u64 {
        (self.max_bytes as f64 * EVICTION_FACTOR) as u64
    }

    /// Record an already-written track in the index: index insert only, no large I/O. The
    /// eviction pass the row owes comes back for the caller to run with the lock free. `remove`
    /// is the unlink an oversized file goes through, passed in for tests to fail it.
    pub(super) fn insert_row(
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
}
