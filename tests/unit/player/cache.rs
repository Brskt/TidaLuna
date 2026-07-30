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
