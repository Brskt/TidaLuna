//! Tests for `src/player/cache.rs`, attached to it by `#[path]`.

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
    // Clock rewound to 50: the stamp must still exceed the existing max; a
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
    assert_eq!(
        cache
            .record_if_current("track", "flac", 3, cur_gen)
            .unwrap(),
        StoreOutcome::Disabled
    );
    assert!(!path.exists());
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

#[test]
fn a_matching_format_generation_keeps_entries() {
    let tmp = tempfile::tempdir().unwrap();
    {
        let mut cache = AudioCache::open_with_capacity(tmp.path(), 1000).unwrap();
        let path = cache.file_path("track");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"ciphertext").unwrap();
        cache.record("track", "flac", 10).unwrap();
    }

    let cache = AudioCache::open_with_capacity(tmp.path(), 1000).unwrap();
    assert!(cache.lookup_path("track").is_some());
    assert_eq!(cache.total_size(), 10);
}

#[test]
fn orphaned_staging_files_are_swept_but_sharded_entries_survive() {
    let tmp = tempfile::tempdir().unwrap();
    let sharded = {
        let mut cache = AudioCache::open_with_capacity(tmp.path(), 1000).unwrap();
        let path = cache.file_path("track");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"ciphertext").unwrap();
        cache.record("track", "flac", 10).unwrap();
        path
    };

    // A staging file left behind by an unclean exit: directly in the audio
    // dir, never indexed.
    let audio_dir = tmp.path().join("cache").join("audio");
    let orphan = audio_dir.join(".tmpAbC123");
    fs::write(&orphan, b"half a download").unwrap();

    let cache = AudioCache::open_with_capacity(tmp.path(), 1000).unwrap();
    assert!(!orphan.exists(), "orphaned staging file must be swept");
    assert!(sharded.exists(), "an indexed entry must survive the sweep");
    assert!(cache.lookup_path("track").is_some());
}

#[test]
fn a_disabled_cache_never_offers_a_staging_dir() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(AudioCache::disabled(tmp.path()).staging_dir().is_none());
    assert!(
        AudioCache::open_with_capacity(tmp.path(), 1000)
            .unwrap()
            .staging_dir()
            .is_some()
    );
}

#[test]
fn dropping_an_entry_clears_the_file_the_row_and_the_accounting() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cache = AudioCache::open_with_capacity(tmp.path(), 1000).unwrap();
    let path = cache.file_path("track");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"ciphertext").unwrap();
    cache.record("track", "flac", 10).unwrap();

    assert_eq!(cache.drop_entry("track"), DropOutcome::Dropped);
    assert!(!path.exists());
    assert_eq!(cache.lookup_path("track"), None);
    assert_eq!(cache.total_size(), 0);
}

/// The row is itself the orphan here: dropping has to succeed rather than
/// refuse for want of a file.
#[test]
fn dropping_an_entry_whose_file_already_vanished_still_clears_the_row() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cache = AudioCache::open_with_capacity(tmp.path(), 1000).unwrap();
    cache.record("track", "flac", 10).unwrap();

    assert_eq!(cache.drop_entry("track"), DropOutcome::Dropped);
    assert_eq!(cache.lookup_path("track"), None);
    assert_eq!(cache.total_size(), 0);
}

/// Nothing indexed and nothing on disk is not a failure, and used to be reported with the
/// same `false` as a file that refused to go.
#[test]
fn dropping_an_entry_that_was_never_recorded_reports_no_row() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cache = AudioCache::open_with_capacity(tmp.path(), 1000).unwrap();
    assert_eq!(cache.drop_entry("absent"), DropOutcome::NoRow);
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

    // Crossing the cap evicts the oldest (b) down to the 90% target.
    cache.record("c", "flac", 40).unwrap();
    assert_eq!(cache.total_size(), 90);
    assert!(cache.lookup_path("b").is_none());

    cache.clear().unwrap();
    assert_eq!(cache.total_size(), 0);
}

#[test]
fn an_entry_larger_than_the_whole_cache_is_refused_not_stored_then_evicted() {
    let tmp = tempfile::tempdir().unwrap();
    // cap 100 -> eviction target 90. 101 is the smallest size no eviction pass can resolve.
    let mut cache = AudioCache::open_with_capacity(tmp.path(), 100).unwrap();
    cache.record("keeper", "flac", 50).unwrap();

    let oversized = cache.file_path("huge");
    fs::create_dir_all(oversized.parent().unwrap()).unwrap();
    fs::write(&oversized, b"pcm").unwrap();

    assert_eq!(
        cache.record("huge", "flac", 101).unwrap(),
        StoreOutcome::TooLarge
    );
    // Refused up front: the staged file is gone, nothing was indexed, and, the point of the
    // admission gate, the rest of the cache survived instead of being evicted to make room
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
#[cfg(unix)]
fn an_undeletable_oversized_entry_does_not_evict_the_rest_of_the_cache() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let mut cache = AudioCache::open_with_capacity(tmp.path(), 100).unwrap();
    cache.record("keeper", "flac", 50).unwrap();

    let oversized = cache.file_path("huge");
    let shard = oversized.parent().unwrap().to_path_buf();
    fs::create_dir_all(&shard).unwrap();
    fs::write(&oversized, b"pcm").unwrap();
    // Dropping write permission on the shard makes the unlink fail with something other than
    // NotFound, the only route to `DropOutcome::FileKept`, and from there to `TooLargeRetained`.
    let original = fs::metadata(&shard).unwrap().permissions();
    fs::set_permissions(&shard, fs::Permissions::from_mode(0o555)).unwrap();

    let outcome = cache.record("huge", "flac", 101);

    // Before any assertion: a failure still leaves a removable tempdir behind.
    fs::set_permissions(&shard, original).unwrap();

    assert_eq!(outcome.unwrap(), StoreOutcome::TooLargeRetained);
    assert!(cache.lookup_path("keeper").is_some());
    assert!(cache.lookup_path("huge").is_some());
    assert_eq!(cache.total_size(), 151);
}

/// The gate must bound on the cap, never on the eviction target. Between the two, eviction
/// never even engages, and this band was cached perfectly well before the gate existed.
#[test]
fn an_entry_between_the_target_and_the_cap_is_still_admitted() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cache = AudioCache::open_with_capacity(tmp.path(), 100).unwrap();

    assert_eq!(cache.record("big", "flac", 95).unwrap(), StoreOutcome::Kept);
    assert!(cache.lookup_path("big").is_some());
    // 95 <= 100; evict_if_needed returns before looking at anything.
    assert_eq!(cache.total_size(), 95);
}

#[test]
fn refusing_an_oversized_entry_clears_a_row_left_from_an_earlier_read_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cache = AudioCache::open_with_capacity(tmp.path(), 100).unwrap();
    // An entry that fails to decrypt is deliberately kept (CacheReadError::Unreadable): a
    // row for this id can still be present when the track is re-fetched at a larger size.
    cache.record("track", "flac", 50).unwrap();
    assert!(cache.lookup_path("track").is_some());
    assert_eq!(cache.total_size(), 50);

    assert_eq!(
        cache.record("track", "flac", 101).unwrap(),
        StoreOutcome::TooLarge
    );
    // Deleting only the file would leave this row reporting a hit for something that is no
    // longer there, with its bytes counted in the running total for good.
    assert!(cache.lookup_path("track").is_none());
    assert_eq!(cache.total_size(), 0);
}

#[test]
fn a_store_is_never_undone_by_the_eviction_it_triggers() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cache = AudioCache::open_with_capacity(tmp.path(), 100).unwrap();
    cache.record("old", "flac", 80).unwrap();

    // 85 fits under the target (90) but pushes the total over the cap; this insert
    // triggers an eviction pass that must not reach the row it just wrote.
    assert_eq!(
        cache.record("fresh", "flac", 85).unwrap(),
        StoreOutcome::Kept
    );
    assert!(cache.lookup_path("fresh").is_some());
    assert!(cache.lookup_path("old").is_none());
    assert_eq!(cache.total_size(), 85);
}
