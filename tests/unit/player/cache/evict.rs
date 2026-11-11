//! Tests for `src/player/cache/evict.rs`, attached to it by `#[path]`.

use super::*;
use crate::player::cache::StoreOutcome;
use crate::player::cache::fixture::staged;
use crate::player::cache::store::hold_moves_off;

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
