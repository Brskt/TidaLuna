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
