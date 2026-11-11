/// Poison is recovered, never propagated: one failing assertion must not turn every later
/// test red for a reason that is not its own.
static SLOT: std::sync::Mutex<()> = std::sync::Mutex::new(());

thread_local! {
    /// Whether this thread holds `SLOT`. The guard is `!Send`, so a holder never moves off
    /// the thread that took it.
    static HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Fails a write to the slot from a thread holding no fixture, at once and in any run: the
/// race it would open shows only in parallel runs, and seldom. Assumes a test's writes run on
/// the test's own thread; a write a test drives from another thread has none of the marker.
#[track_caller]
pub(super) fn assert_held(write: &str) {
    assert!(
        HELD.get(),
        "{write} on CURRENT_TRACK without a CurrentTrackSlot held by this thread"
    );
}

/// Lock ORDER where a test holds both: `PRELOAD_TESTS` first, this one second. Taking
/// them the other way round deadlocks against every test that already holds both.
pub(crate) struct CurrentTrackSlot {
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl CurrentTrackSlot {
    fn take() -> Self {
        let slot = Self {
            _lock: SLOT.lock().unwrap_or_else(|e| e.into_inner()),
        };
        HELD.set(true);
        slot
    }

    /// The slot empty, for a test that needs no track published.
    pub(crate) fn clear() -> Self {
        let slot = Self::take();
        *super::CURRENT_TRACK
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = super::CurrentSource::new();
        slot
    }

    /// The slot holding a published track, for a test that needs one to exist rather than
    /// to be absent. Same exclusion, opposite starting state.
    pub(crate) fn holding(retained: super::RetainedTrack) -> Self {
        let slot = Self::take();
        super::CURRENT_TRACK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publish_load(retained.load_gen, Some(retained.track));
        slot
    }
}

/// Restores the slot even when the test panics, keeping a failure from leaking into
/// whichever test the harness runs next.
impl Drop for CurrentTrackSlot {
    fn drop(&mut self) {
        *super::CURRENT_TRACK
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = super::CurrentSource::new();
        HELD.set(false);
    }
}
