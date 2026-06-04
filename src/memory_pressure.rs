//! Render-process image-cache reclamation.
//!
//! On a RAM-rich Windows host the OS never sends `base::MemoryPressureListener`
//! a pressure signal, so decoded artwork bitmaps accumulate unbounded. We raise
//! it ourselves over CDP (`Memory.simulatePressureNotification`) to free the
//! unreferenced bitmaps; the visible cover stays ref-held and is kept.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use cef::*;

use crate::app_state::with_state;

const METHOD_SIMULATE_PRESSURE: i32 = 3;

// Rapid track skips fire `player.load` in quick succession; throttle so a skip
// storm triggers at most one purge per interval instead of one per skip.
const MIN_PURGE_INTERVAL_MS: u64 = 1500;

static PROCESS_START: LazyLock<Instant> = LazyLock::new(Instant::now);
static LAST_PURGE_MS: AtomicU64 = AtomicU64::new(0);

wrap_task! {
    struct PurgeTask {
        _p: u8,
    }
    impl Task {
        fn execute(&self) {
            let Some(browser) = with_state(|s| s.browser.clone()).flatten() else {
                return;
            };
            let Some(host) = browser.host() else { return };
            let Some(mut params) = dictionary_value_create() else {
                return;
            };
            params.set_string(
                Some(&CefString::from("level")),
                Some(&CefString::from("critical")),
            );
            let _ = host.execute_dev_tools_method(
                METHOD_SIMULATE_PRESSURE,
                Some(&CefString::from("Memory.simulatePressureNotification")),
                Some(&mut params),
            );
        }
    }
}

/// Ask Chromium to drop unreferenced decoded-image / discardable memory in the
/// render process. Safe to call from any thread and on every track change; the
/// work is marshalled to the UI thread and throttled to [`MIN_PURGE_INTERVAL_MS`].
pub fn purge_image_cache() {
    let now = PROCESS_START.elapsed().as_millis() as u64;
    if !try_claim_purge(&LAST_PURGE_MS, now) {
        return;
    }
    let mut task = PurgeTask::new(0);
    post_task(ThreadId::UI, Some(&mut task));
}

/// Claim the purge slot with one atomic `fetch_update` so concurrent callers
/// can't both pass the throttle and double-purge. True iff this caller won.
fn try_claim_purge(last_purge: &AtomicU64, now: u64) -> bool {
    last_purge
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
            // now.max(1) keeps stored values off the `last == 0` "never purged" sentinel.
            if last != 0 && now.saturating_sub(last) < MIN_PURGE_INTERVAL_MS {
                None
            } else {
                Some(now.max(1))
            }
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_purge_wins_from_sentinel() {
        let last = AtomicU64::new(0);
        assert!(try_claim_purge(&last, 500));
    }

    #[test]
    fn second_purge_within_interval_is_throttled() {
        let last = AtomicU64::new(0);
        assert!(try_claim_purge(&last, 1000));
        assert!(!try_claim_purge(&last, 1000 + MIN_PURGE_INTERVAL_MS - 1));
    }

    #[test]
    fn purge_after_interval_wins_again() {
        let last = AtomicU64::new(0);
        assert!(try_claim_purge(&last, 1000));
        assert!(try_claim_purge(&last, 1000 + MIN_PURGE_INTERVAL_MS));
    }

    #[test]
    fn concurrent_claims_only_one_wins() {
        use std::sync::{Arc, Barrier};
        let last = Arc::new(AtomicU64::new(1000));
        let now = 1000 + MIN_PURGE_INTERVAL_MS; // throttle window just open
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let last = last.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    try_claim_purge(&last, now)
                })
            })
            .collect();
        let wins = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|&won| won)
            .count();
        assert_eq!(wins, 1);
    }
}
