//! Tests for `src/video/cover_cache.rs`, attached by `#[path]`.

use super::*;

fn entry(name: &str, len: u64, used: SystemTime) -> CacheEntry {
    CacheEntry {
        path: PathBuf::from(name),
        len,
        used,
    }
}

fn cover_name(n: u8) -> String {
    format!("{}.webm", format!("{n:x}").repeat(64))
}

/// Nothing else bounds the directory: covers go oldest-used first until the rest fits, and a
/// temporary file a crash left behind goes too, once it is old enough not to be a store in flight.
#[test]
fn eviction_keeps_the_newest_covers_under_budget_and_sweeps_stale_leftovers() {
    let now = SystemTime::now();
    let ago = |secs: u64| now - Duration::from_secs(secs);
    let entries = vec![
        entry(&cover_name(1), 40, ago(30)),
        entry(&cover_name(2), 40, ago(10)),
        entry(&cover_name(3), 40, ago(20)),
        entry(".tmpOld", 5, ago(3_600)),
        entry(".tmpNew", 5, ago(5)),
    ];

    let doomed = plan_eviction(entries, 100, now);

    assert_eq!(
        doomed,
        [PathBuf::from(".tmpOld"), PathBuf::from(cover_name(1))],
        "the stale leftover, then the least recently used cover"
    );
}

/// A cover an older format wrote is still valid WebM; only its name keeps it from being served,
/// and it stays a cover for eviction to take.
#[test]
fn a_cover_from_an_older_format_is_not_served() {
    let dir = tempfile::tempdir().unwrap();
    let key = CoverKey::parse("https://resources.tidal.com/videos/a/640x640.mp4", "640").unwrap();
    let old_name = crate::video::codec::sha256_bytes(key.url().as_bytes());
    let old = dir.path().join(format!("{old_name}.webm"));
    std::fs::write(&old, b"old").unwrap();
    let cache = CoverCache::new(dir.path().to_path_buf(), BUDGET_BYTES);

    assert_eq!(cache.get(&key), None, "the older file is not this format's");
    assert!(is_cover_file(&old), "eviction still counts and takes it");
    cache.put(&key, b"new").unwrap();
    assert_eq!(cache.get(&key).as_deref(), Some(&b"new"[..]));
}

/// The Clear runs beside the stores: deleting a staging file a store is still writing fails its
/// rename, and the page reads that as the cover's fault.
#[test]
fn a_clear_leaves_the_file_a_store_is_still_writing() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, age: u64| {
        let path = dir.path().join(name);
        std::fs::write(&path, b"webm").unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(age))
            .unwrap();
    };
    write(&cover_name(1), 0);
    write(".tmpLive", 5);
    write(".tmpOld", 3_600);

    CoverCache::new(dir.path().to_path_buf(), BUDGET_BYTES).clear();

    let left: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(left, [".tmpLive"]);
}
