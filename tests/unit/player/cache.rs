//! Tests for `src/player/cache/mod.rs`, attached to it by `#[path]`.

use super::fixture::wipe;
use super::*;

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
