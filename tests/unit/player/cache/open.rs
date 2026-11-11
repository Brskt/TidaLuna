//! Tests for `src/player/cache/open.rs`, attached to it by `#[path]`.

use super::*;
use crate::player::cache::Generation;
use crate::player::cache::fixture::generation_of;

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
