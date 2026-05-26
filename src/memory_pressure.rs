//! Render-process image-cache reclamation.
//!
//! Chromium evicts decoded cover/background bitmaps (cc `gpu_image_decode_cache`
//! plus the discardable-memory pool) only when `base::MemoryPressureListener`
//! receives an OS pressure signal. On a 64-bit Windows host with abundant free
//! RAM that signal never fires, so the per-track artwork bitmaps accumulate
//! without bound. We already drive the renderer over CDP, so we raise the signal
//! ourselves: `Memory.simulatePressureNotification` calls
//! `MemoryPressureListener::SimulatePressureNotification`, which notifies the
//! native listeners (image-decode cache, discardable-memory manager) and frees
//! the unreferenced bitmaps. The currently visible cover stays ref-held by the
//! live layer tree, so it is not dropped.

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
    let last = LAST_PURGE_MS.load(Ordering::Relaxed);
    // last == 0 means "never purged" - always let the first one through (it can
    // land in the first 1.5s of the process). Store now.max(1) so the sentinel
    // can't recur and re-open the gate.
    if last != 0 && now.saturating_sub(last) < MIN_PURGE_INTERVAL_MS {
        return;
    }
    LAST_PURGE_MS.store(now.max(1), Ordering::Relaxed);

    let mut task = PurgeTask::new(0);
    post_task(ThreadId::UI, Some(&mut task));
}
