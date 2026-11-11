//! Tests for `src/player/cache/checkpoint.rs`, attached to it by `#[path]`.

use crate::player::cache::{AudioCache, checkpoint};

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
