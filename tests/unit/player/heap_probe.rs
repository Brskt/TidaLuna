//! The unit-test binary's global allocator: the system one, counting what the CURRENT thread asks
//! of it while a probe is armed. Per thread (parallel tests never see each other's allocations),
//! and a count rather than an abort (a violation fails one test, not the whole binary).
//!
//! The state is `const`-initialised and has no `Drop`: reading it from inside the allocator can
//! then neither allocate nor register a thread-exit destructor, either of which would recurse.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static EVENTS: Cell<usize> = const { Cell::new(0) };
}

struct CountingSystem;

fn note() {
    // `try_with`: a thread tearing down its locals still frees, and must not panic in here.
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            EVENTS.with(|events| events.set(events.get() + 1));
        }
    });
}

// SAFETY: every call is forwarded unchanged to `System`, which upholds the `GlobalAlloc`
// contract; the counting touches only `const` thread-locals and never allocates.
unsafe impl GlobalAlloc for CountingSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: forwarded with the caller's own guarantees on `layout`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: as `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note();
        // SAFETY: `ptr` came from this allocator, which is `System`, with this `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: as `dealloc`, with the caller's guarantees on `new_size`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING_SYSTEM: CountingSystem = CountingSystem;

/// Disarms on the way out, a panicking `f` included.
struct Armed;

impl Drop for Armed {
    fn drop(&mut self) {
        ARMED.with(|armed| armed.set(false));
    }
}

/// How many allocations, reallocations and frees `f` made on this thread. Frees count as much as
/// allocations: both reach the allocator, whose latency is what a real-time thread cannot afford.
pub(crate) fn heap_calls_during(f: impl FnOnce()) -> usize {
    EVENTS.with(|events| events.set(0));
    ARMED.with(|armed| armed.set(true));
    let guard = Armed;
    f();
    drop(guard);
    EVENTS.with(|events| events.get())
}
