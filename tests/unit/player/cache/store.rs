//! Tests for `src/player/cache/store.rs`, attached to it by `#[path]`.

use super::*;
use crate::player::cache::HELD;
use crate::player::cache::fixture::{generation_of, staged, wipe};

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
