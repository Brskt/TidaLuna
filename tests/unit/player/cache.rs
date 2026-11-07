//! Tests for `src/player/cache.rs`, attached to it by `#[path]`.

use super::*;

/// The calls the production passes make over a shared cache, for a test's own cache: lent to a
/// `Mutex` for the length of the call, as `store_into` and the applier hold the global one.
impl AudioCache {
    fn lent<R>(&mut self, run: impl FnOnce(&Mutex<AudioCache>) -> R) -> R {
        let placeholder = AudioCache::disabled(Path::new(""), 0);
        let shared = Mutex::new(std::mem::replace(self, placeholder));
        let out = run(&shared);
        *self = shared.into_inner().unwrap_or_else(|e| e.into_inner());
        out
    }

    /// `insert_row` and the pass it owes, as `store_into` runs them.
    pub(super) fn record(
        &mut self,
        track_id: &str,
        format: &str,
        file_size: u64,
    ) -> anyhow::Result<StoreOutcome> {
        self.record_with(track_id, format, file_size, &|p| fs::remove_file(p))
    }

    fn record_with(
        &mut self,
        track_id: &str,
        format: &str,
        file_size: u64,
        remove: &dyn Fn(&Path) -> std::io::Result<()>,
    ) -> anyhow::Result<StoreOutcome> {
        let (outcome, owed) = self.insert_row(track_id, format, file_size, remove)?;
        self.run_owed(owed, remove)?;
        Ok(outcome)
    }

    /// `commit_row` and the pass it owes, as `store_into` runs them.
    fn commit(
        &mut self,
        reservation: Reservation,
        format: &str,
        file_size: u64,
        moved: anyhow::Result<()>,
    ) -> anyhow::Result<StoreOutcome> {
        let (outcome, owed) = self.commit_row(reservation, format, file_size, moved)?;
        self.run_owed(owed, &|p| fs::remove_file(p))?;
        Ok(outcome)
    }

    fn run_owed(
        &mut self,
        owed: Option<Owed>,
        remove: &dyn Fn(&Path) -> std::io::Result<()>,
    ) -> Result<(), CacheError> {
        let Some(owed) = owed else {
            return Ok(());
        };
        self.lent(|cache| AudioCache::evict(cache, Some(&owed.keep), owed.unreclaimable, remove))
    }
}

/// A Clear as the applier runs it: the move gate taken first, then the wipe.
fn wipe(cache: &mut AudioCache) -> Result<(), CacheError> {
    let moves_off = hold_moves_off();
    cache.lent(|cache| AudioCache::wipe(cache, &moves_off, &|p| fs::remove_file(p)))
}

/// A staging file in `dir`, holding `bytes`: what a finished download hands over.
fn staged(dir: &Path, bytes: &[u8]) -> (tempfile::NamedTempFile, PathBuf) {
    use std::io::Write;
    fs::create_dir_all(dir).unwrap();
    let mut file = tempfile::NamedTempFile::new_in(dir).unwrap();
    file.write_all(bytes).unwrap();
    let path = file.path().to_path_buf();
    (file, path)
}

#[test]
fn a_stale_format_generation_drops_every_entry() {
    let tmp = tempfile::tempdir().unwrap();

    let path = {
        let mut cache = AudioCache::open_with_capacity(tmp.path(), 1000).unwrap();
        let path = cache.file_path("track");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"plaintext flac").unwrap();
        cache.record("track", "flac", 14).unwrap();
        assert!(cache.lookup_path("track").is_some());
        path
    };

    // Pose as a pre-versioning database - the plaintext generation.
    Connection::open(tmp.path().join("cache").join("index.db"))
        .unwrap()
        .pragma_update(None, "user_version", 0i64)
        .unwrap();

    let cache = AudioCache::open_with_capacity(tmp.path(), 1000).unwrap();
    assert_eq!(cache.lookup_path("track"), None);
    assert!(!path.exists());
    assert_eq!(cache.total_size(), 0);
}

/// What an unclean exit leaves: a staging file never indexed, and a file whose row went first
/// (the crash between the two). The next open frees both and keeps what a row describes.
#[test]
fn open_sweeps_what_no_row_describes() {
    let tmp = tempfile::tempdir().unwrap();
    let orphan;
    {
        let mut cache = AudioCache::open_with_capacity(tmp.path(), 10_000).unwrap();
        cache.record("kept", "flac", 10).unwrap();
        cache.record("orphan", "flac", 10).unwrap();
        for id in ["kept", "orphan"] {
            let path = cache.file_path(id);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"x").unwrap();
        }
        orphan = cache.file_path("orphan");
        let seen = cache.lookup_seen("orphan").unwrap().generation;
        cache.remove_orphan("orphan", seen);
    }
    let staging = tmp.path().join("cache").join("audio").join(".tmpAbC123");
    fs::write(&staging, b"half a download").unwrap();

    let cache = AudioCache::open_with_capacity(tmp.path(), 10_000).unwrap();
    assert!(!staging.exists());
    assert!(!orphan.exists());
    assert!(cache.file_path("kept").exists());
    assert!(cache.lookup_path("kept").is_some());
}

/// A row whose file already vanished is itself the orphan: dropping it has to succeed rather than
/// refuse for want of a file.
#[test]
fn dropping_an_entry_clears_its_row_and_accounting_and_its_file_when_it_has_one() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cache = AudioCache::open_with_capacity(tmp.path(), 1000).unwrap();
    let path = cache.file_path("with-file");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"ciphertext").unwrap();
    cache.record("with-file", "flac", 10).unwrap();
    cache.record("vanished", "flac", 10).unwrap();

    for id in ["with-file", "vanished"] {
        let seen = cache.lookup_seen(id).unwrap().generation;
        assert_eq!(cache.drop_seen(id, seen), DropOutcome::Dropped, "{id}");
        assert_eq!(cache.lookup_path(id), None);
    }
    assert!(!path.exists());
    assert_eq!(cache.total_size(), 0);
}

#[test]
fn running_size_tracks_inserts_evictions_and_clear() {
    let tmp = tempfile::tempdir().unwrap();
    // Small cap for a few inserts to cross it and trigger eviction (target = 90).
    let mut cache = AudioCache::open_with_capacity(tmp.path(), 100).unwrap();
    assert_eq!(cache.total_size(), 0);

    cache.record("a", "flac", 30).unwrap();
    cache.record("b", "flac", 40).unwrap();
    assert_eq!(cache.total_size(), 70);

    // Re-recording an existing id replaces it: the total moves by the delta.
    cache.record("a", "flac", 50).unwrap();
    assert_eq!(cache.total_size(), 90);
    // Played again: now the most recent, though stored before `a` was re-recorded.
    cache.touch("b");

    // Crossing the cap evicts the least recently played (a) down to the 90% target.
    cache.record("c", "flac", 40).unwrap();
    assert_eq!(cache.total_size(), 80);
    assert!(cache.lookup_path("a").is_none());
    assert!(cache.lookup_path("b").is_some());

    wipe(&mut cache).unwrap();
    assert_eq!(cache.total_size(), 0);
}

/// An entry that fails to decrypt is kept (`CacheReadError::Unreadable`), so a row for the id can
/// still be there when the track is re-fetched at a size no pass can make room for.
#[test]
fn an_entry_larger_than_the_whole_cache_is_refused_and_leaves_the_rest_and_no_row_of_its_own() {
    let tmp = tempfile::tempdir().unwrap();
    // cap 100 -> eviction target 90. 101 is the smallest size no eviction pass can resolve.
    let mut cache = AudioCache::open_with_capacity(tmp.path(), 100).unwrap();
    cache.record("keeper", "flac", 50).unwrap();
    cache.record("huge", "flac", 40).unwrap();

    let oversized = cache.file_path("huge");
    fs::create_dir_all(oversized.parent().unwrap()).unwrap();
    fs::write(&oversized, b"pcm").unwrap();

    assert_eq!(
        cache.record("huge", "flac", 101).unwrap(),
        StoreOutcome::TooLarge
    );
    // Refused up front, the rest of the cache survives instead of being evicted to make room
    // for a file that would have been deleted anyway.
    assert!(!oversized.exists());
    assert!(cache.lookup_path("huge").is_none());
    assert!(cache.lookup_path("keeper").is_some());
    assert_eq!(cache.total_size(), 50);
}

/// An oversized entry whose file will not delete is indexed at its true size. The eviction
/// pass its own store triggers is asked for a target that row makes unreachable. Excluded from
/// the candidates, its bytes are not reclaimable either: counting them in ground the pass
/// through every other row and finished over the cap regardless, an empty cache for nothing.
#[test]
fn an_undeletable_oversized_entry_is_indexed_at_its_true_size() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cache = AudioCache::open_with_capacity(tmp.path(), 100).unwrap();
    cache.record("keeper", "flac", 50).unwrap();

    let oversized = cache.file_path("huge");
    fs::create_dir_all(oversized.parent().unwrap()).unwrap();
    fs::write(&oversized, b"pcm").unwrap();
    // An unlink that fails with something other than NotFound is the only route to
    // `DropOutcome::FileKept`, and from there to `TooLargeRetained`.
    let outcome = cache.record_with("huge", "flac", 101, &|_| {
        Err(std::io::ErrorKind::PermissionDenied.into())
    });

    assert_eq!(outcome.unwrap(), StoreOutcome::TooLargeRetained);
    assert!(cache.lookup_path("keeper").is_some());
    assert!(cache.lookup_path("huge").is_some());
    assert_eq!(cache.total_size(), 151);
    assert_eq!(cache.untrimmed(), Some(HELD));
}

/// What lands between the reservation and the row decides the store: the switch going off after
/// the move discards the moved file, a Clear after the move takes it, a Clear before the move
/// leaves the staging file alone and the store lands after it.
#[test]
fn a_store_follows_what_ran_between_its_move_and_its_row() {
    #[derive(Debug, Clone, Copy)]
    enum Between {
        SwitchOff,
        ClearAfterMove,
        ClearBeforeMove,
    }
    for (between, expected) in [
        (Between::SwitchOff, StoreOutcome::Disabled),
        (Between::ClearAfterMove, StoreOutcome::ClearedMidWrite),
        (Between::ClearBeforeMove, StoreOutcome::Kept),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let mut cache = AudioCache::open_with_capacity(tmp.path(), 1000).unwrap();
        let (file, _) = staged(&cache.audio_dir, b"ciphertext");
        let reservation = cache.reserve("track").unwrap();
        let path = reservation.path.clone();
        if let Between::ClearBeforeMove = between {
            wipe(&mut cache).unwrap();
        }
        let moved = reservation.move_in(file);
        match between {
            Between::SwitchOff => cache.set_enabled(false),
            Between::ClearAfterMove => wipe(&mut cache).unwrap(),
            Between::ClearBeforeMove => {}
        }

        let outcome = cache.commit(reservation, "flac", 10, moved).unwrap();
        assert_eq!(outcome, expected, "{between:?}");
        cache.set_enabled(true);
        let landed = outcome == StoreOutcome::Kept;
        assert_eq!(cache.lookup_path("track").is_some(), landed, "{between:?}");
        assert_eq!(path.exists(), landed, "{between:?}");
        assert_eq!(cache.reserve("track").is_ok(), !landed, "{between:?}");
    }
}

/// A second download of an id whose first is mid-move is refused, and the refusal does not
/// release the first's reservation.
#[test]
fn a_second_store_of_an_id_mid_move_is_refused_and_the_first_still_lands() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cache = AudioCache::open_with_capacity(tmp.path(), 1000).unwrap();
    let (first, _) = staged(&cache.audio_dir, b"ciphertext");
    let reservation = cache.reserve("track").unwrap();
    assert_eq!(
        cache.reserve("track").unwrap_err(),
        StoreOutcome::AlreadyCached
    );
    let moved = reservation.move_in(first);
    assert_eq!(
        cache.commit(reservation, "flac", 10, moved).unwrap(),
        StoreOutcome::Kept
    );
    assert_eq!(
        cache.reserve("track").unwrap_err(),
        StoreOutcome::AlreadyCached
    );
}

#[test]
fn a_store_is_never_undone_by_the_eviction_it_triggers() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cache = AudioCache::open_with_capacity(tmp.path(), 100).unwrap();
    cache.record("old", "flac", 20).unwrap();

    // 95 is admitted under the cap but pushes the total over it. Evicting `old` still
    // leaves the total over the target (90): the pass goes on, and must not reach the
    // row this insert just wrote.
    assert_eq!(
        cache.record("fresh", "flac", 95).unwrap(),
        StoreOutcome::Kept
    );
    assert!(cache.lookup_path("fresh").is_some());
    assert!(cache.lookup_path("old").is_none());
    assert_eq!(cache.total_size(), 95);
}

/// A publisher over a scratch cache, and what it published.
fn publishing(
    cache: &std::sync::Arc<std::sync::Mutex<AudioCache>>,
) -> (mpsc::Receiver<CacheState>, mpsc::SyncSender<()>) {
    let (out_tx, out_rx) = mpsc::channel();
    let weak = std::sync::Arc::downgrade(cache);
    let on_change = spawn_publisher(
        move || weak.upgrade().map(|cache| cache.lock().unwrap().state()),
        move |state| {
            let _ = out_tx.send(*state);
        },
    )
    .unwrap();
    (out_rx, on_change)
}

const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Coalescing loses no state: a burst ends on its last value.
#[test]
fn a_burst_of_changes_converges_on_the_last_state() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = std::sync::Arc::new(std::sync::Mutex::new(
        AudioCache::open_with_capacity(tmp.path(), 1_000_000).unwrap(),
    ));
    let (published, on_change) = publishing(&cache);
    cache.lock().unwrap().watch(on_change);

    for n in 0..200 {
        cache
            .lock()
            .unwrap()
            .record(&format!("track-{n}"), "flac", 10)
            .unwrap();
    }

    while published
        .recv_timeout(WAIT)
        .expect("the final state arrives")
        .used_bytes
        != 200 * 10
    {}
}

/// `n` entries of 100 bytes, each with its file, stored the way a download is.
fn filled(cache: &Mutex<AudioCache>, n: usize) {
    for i in 0..n {
        let id = format!("t{i}");
        let dir = cache.lock().unwrap().audio_dir.clone();
        let (file, _) = staged(&dir, id.as_bytes());
        assert_eq!(
            AudioCache::store_into(cache, &id, "flac", file, 100).unwrap(),
            StoreOutcome::Kept
        );
    }
}

/// The first step of a pass over a cache that is over its cap, keeping nothing.
fn trim_all() -> Goal<'static> {
    Goal::Trim {
        keep: None,
        unreclaimable: 0,
        first: true,
    }
}

/// An id an eviction has taken the row of has no row and still has its old file on its path: a
/// store admitted then would have its new bytes unlinked by the pass.
#[test]
fn a_store_of_an_id_being_evicted_is_refused_until_its_file_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = Mutex::new(AudioCache::open_with_capacity(tmp.path(), 10_000).unwrap());
    filled(&cache, 3);
    let mut locked = cache.lock().unwrap();
    locked.max_bytes = 200;
    let mut cursor = Cursor::default();
    let batch = locked.claim(trim_all(), &mut cursor).unwrap();
    let id = batch[0].id.clone();
    assert!(matches!(
        locked.reserve(&id),
        Err(StoreOutcome::AlreadyCached)
    ));
    let done = batch.iter().map(|_| Ok(())).collect();
    locked.settle_unlinks(batch, done);
    assert!(locked.reserve(&id).is_ok());
}

/// A Clear that meets a file it cannot delete frees the rest and fails with that file's row still
/// beside it, its bytes counted.
#[test]
fn a_clear_that_cannot_delete_a_file_frees_the_rest_and_keeps_that_row_beside_its_file() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = Mutex::new(AudioCache::open_with_capacity(tmp.path(), 100_000).unwrap());
    // More than a batch, the refused one in the second.
    filled(&cache, EVICTION_BATCH as usize + 6);
    let stubborn = cache.lock().unwrap().file_path("t66");
    let moves_off = hold_moves_off();
    let outcome = AudioCache::wipe(&cache, &moves_off, &|path| {
        if path == stubborn {
            Err(std::io::ErrorKind::PermissionDenied.into())
        } else {
            fs::remove_file(path)
        }
    });
    drop(moves_off);

    assert!(matches!(outcome, Err(CacheError::Disk(_))));
    let locked = cache.lock().unwrap();
    assert!(!locked.wiping && locked.doomed.is_empty());
    assert_eq!(locked.total_size(), 100);
    assert!(locked.file_path("t66").exists());
    assert!(locked.lookup_path("t66").is_some());
    for i in (0..70).filter(|i| *i != 66) {
        let id = format!("t{i}");
        assert!(locked.lookup_path(&id).is_none() && !locked.file_path(&id).exists());
    }
}

/// A pass that has taken rows out and not yet unlinked them owns their files: a Clear in between
/// finds no row for them and must still leave them alone, or the row the pass puts back for a
/// file it could not unlink describes a file the Clear took.
#[test]
fn a_clear_leaves_the_files_of_an_eviction_in_flight_to_that_eviction() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = Mutex::new(AudioCache::open_with_capacity(tmp.path(), 10_000).unwrap());
    filled(&cache, 3);
    let (batch, path) = {
        let mut locked = cache.lock().unwrap();
        locked.max_bytes = 250;
        let mut cursor = Cursor::default();
        let batch = locked.claim(trim_all(), &mut cursor).unwrap();
        let path = batch[0].path.clone();
        (batch, path)
    };

    let moves_off = hold_moves_off();
    AudioCache::wipe(&cache, &moves_off, &|p| fs::remove_file(p)).unwrap();
    drop(moves_off);
    assert!(
        path.exists(),
        "the Clear took the file of an eviction's batch"
    );

    let refused = batch
        .iter()
        .map(|_| Err(std::io::ErrorKind::PermissionDenied.into()))
        .collect();
    let mut locked = cache.lock().unwrap();
    locked.settle_unlinks(batch, refused);
    assert!(locked.lookup_path("t0").is_some());
    assert_eq!(locked.total_size(), 100);
}

/// A download moved into place before the Clear took the gate and committed while it runs is
/// indexed nowhere, and its file goes with the Clear's sweep: a row for it would outlive the file.
#[test]
fn a_download_committing_during_a_clear_is_not_indexed_and_its_file_is_swept() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = Mutex::new(AudioCache::open_with_capacity(tmp.path(), 10_000).unwrap());
    filled(&cache, 1);
    let audio_dir = cache.lock().unwrap().audio_dir.clone();
    let (file, _) = staged(&audio_dir, b"ciphertext");
    let reservation = cache.lock().unwrap().reserve("late").unwrap();
    let path = reservation.path.clone();
    let moved = reservation.move_in(file);
    let waiting = Mutex::new(Some((reservation, moved)));
    let outcome = Mutex::new(None);

    let moves_off = hold_moves_off();
    AudioCache::wipe(&cache, &moves_off, &|p| {
        if let Some((reservation, moved)) = waiting.lock().unwrap().take() {
            let committed = cache
                .lock()
                .unwrap()
                .commit_row(reservation, "flac", 10, moved)
                .map(|(outcome, _)| outcome)
                .unwrap();
            *outcome.lock().unwrap() = Some(committed);
        }
        fs::remove_file(p)
    })
    .unwrap();
    drop(moves_off);

    assert_eq!(
        outcome.into_inner().unwrap(),
        Some(StoreOutcome::ClearedMidWrite)
    );
    let locked = cache.lock().unwrap();
    assert!(locked.lookup_path("late").is_none());
    assert!(!path.exists());
    assert_eq!(locked.total_size(), 0);
}

fn generation_of(cache: &AudioCache, id: &str) -> Generation {
    cache.lookup_seen(id).expect("the id has a row").generation
}

/// A store of `id` through the real `store_into`, as the download path runs it.
fn store_again(cache: &mut AudioCache, id: &str, bytes: &[u8]) -> StoreOutcome {
    let (file, _) = staged(&cache.audio_dir, bytes);
    let len = bytes.len() as u64;
    cache
        .lent(|shared| AudioCache::store_into(shared, id, "flac", file, len))
        .unwrap()
}

/// `id` stored with `bytes` on its path, as a finished download leaves it.
fn stored(cache: &mut AudioCache, id: &str, bytes: &[u8]) -> PathBuf {
    let path = cache.file_path(id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    cache.record(id, "flac", bytes.len() as u64).unwrap();
    path
}

/// A reader's verdict (file gone, or a decode failure up to a track later) is about the entry it
/// read. A Clear and a store of the same id fit before it lands, and the new entry must stand.
#[test]
fn a_verdict_taken_before_a_clear_and_a_store_leaves_the_new_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cache = AudioCache::open_with_capacity(tmp.path(), 1000).unwrap();
    let path = stored(&mut cache, "t", &[b'A'; 60]);
    let seen = generation_of(&cache, "t");

    wipe(&mut cache).unwrap();
    assert_eq!(
        store_again(&mut cache, "t", &[b'C'; 20]),
        StoreOutcome::Kept
    );

    assert!(!cache.remove_orphan("t", seen));
    let outcome = cache.lent(|shared| AudioCache::drop_after_decode_failure(shared, "t", seen));
    assert_eq!(outcome, DropOutcome::NoRow);
    assert!(cache.lookup_path("t").is_some());
    assert_eq!(fs::read(&path).unwrap(), [b'C'; 20]);
}

/// An index written before generations keeps its rows: the column is added in place (the format
/// version would wipe the cache), old rows are generation 0 and later stores go above.
#[test]
fn an_index_from_before_generations_keeps_its_rows_and_hands_out_new_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("cache");
    fs::create_dir_all(&cache_dir).unwrap();
    {
        let conn = Connection::open(cache_dir.join("index.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE audio_cache (
                track_id     TEXT PRIMARY KEY,
                format       TEXT NOT NULL,
                file_size    INTEGER NOT NULL,
                created_at   INTEGER NOT NULL,
                last_access  INTEGER NOT NULL,
                access_count INTEGER DEFAULT 1
            );
            INSERT INTO audio_cache VALUES ('old-a', 'flac', 10, 1, 1, 1);
            INSERT INTO audio_cache VALUES ('old-b', 'flac', 10, 2, 2, 1);",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", CACHE_FORMAT_VERSION)
            .unwrap();
    }

    let mut cache = AudioCache::open_with_capacity(tmp.path(), 1000).unwrap();
    assert_eq!(cache.total_size(), 20);
    assert_eq!(generation_of(&cache, "old-a"), Generation(0));
    assert_eq!(generation_of(&cache, "old-b"), Generation(0));
    cache.record("new", "flac", 10).unwrap();
    assert!(generation_of(&cache, "new").0 > 0);
}

/// The log's length and how much of it is in the database, counted without copying anything.
fn log_of(cache: &AudioCache) -> (i64, i64) {
    cache
        .conn
        .as_ref()
        .unwrap()
        .query_row("PRAGMA wal_checkpoint(NOOP)", [], |row| {
            Ok((row.get(1)?, row.get(2)?))
        })
        .unwrap()
}

/// Nothing else checkpoints the log once the automatic one is off: a thread that never started,
/// or a hook that never rings it, would let the log grow with every write, and no write would
/// say so.
#[test]
fn a_log_past_the_threshold_is_checkpointed_by_the_cache_thread() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cache = AudioCache::open_with_capacity(tmp.path(), 1 << 40).unwrap();

    // The commit that brings the log to the threshold rings the thread, and only the next write
    // restarts the log: writing on past it races the checkpoint, which fast storage wins.
    let mut stored = 0;
    while log_of(&cache).0 < i64::from(checkpoint::WAL_PAGES) {
        assert!(stored < 5000, "the log never reached the threshold");
        cache.record(&format!("track-{stored}"), "flac", 1).unwrap();
        stored += 1;
    }

    let waiting_since = std::time::Instant::now();
    loop {
        let (log, copied) = log_of(&cache);
        if copied == log {
            break;
        }
        assert!(
            waiting_since.elapsed() < crate::state::HANG_GUARD,
            "{copied} of {log} pages of the log copied after {:?}",
            waiting_since.elapsed()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
