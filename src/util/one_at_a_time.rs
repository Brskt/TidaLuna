use std::sync::atomic::{AtomicBool, Ordering};

/// Held while the one task `flag` stands for runs; a second is refused until this one is dropped,
/// by returning or by panicking.
pub(crate) struct OneAtATime(&'static AtomicBool);

impl OneAtATime {
    pub(crate) fn try_acquire(flag: &'static AtomicBool) -> Option<Self> {
        (!flag.swap(true, Ordering::AcqRel)).then_some(Self(flag))
    }
}

impl Drop for OneAtATime {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
