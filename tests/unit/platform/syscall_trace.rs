//! Records, per thread, the `fsync` and `unlink` calls the code under test makes through libc.
//!
//! `fsync` and `unlink` are defined here for `std` (statically linked into the test binary) to
//! resolve to these instead of libc's, and they forward to libc's. A directory flush is a
//! syscall with no visible effect on a healthy disk: only seeing the call proves it was made.
//! `rename` is not traced: `tempfile` reaches it through `rustix`'s raw syscalls, past libc.

use std::cell::RefCell;
use std::ffi::{CStr, c_char, c_int};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    Fsync { path: PathBuf, directory: bool },
    Unlink(PathBuf),
}

thread_local! {
    static LOG: RefCell<Option<Vec<Event>>> = const { RefCell::new(None) };
}

/// Runs `f`, returning its result and the calls it made on this thread, in order.
pub(crate) fn traced<T>(f: impl FnOnce() -> T) -> (T, Vec<Event>) {
    LOG.with(|log| *log.borrow_mut() = Some(Vec::new()));
    let out = f();
    let events = LOG.with(|log| log.borrow_mut().take()).unwrap_or_default();
    (out, events)
}

fn record(event: impl FnOnce() -> Event) {
    let _ = LOG.try_with(|log| {
        if let Ok(mut log) = log.try_borrow_mut()
            && let Some(events) = log.as_mut()
        {
            events.push(event());
        }
    });
}

/// The libc function of this name that the definitions below shadow.
fn next_in_line(name: &CStr) -> usize {
    // SAFETY: `name` is NUL-terminated, and `RTLD_NEXT` asks for the next definition after this
    // binary's own.
    let found = unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr()) };
    assert!(!found.is_null(), "libc has no {name:?}");
    found as usize
}

#[unsafe(no_mangle)]
pub extern "C" fn fsync(fd: c_int) -> c_int {
    record(|| {
        let link = format!("/proc/self/fd/{fd}");
        Event::Fsync {
            path: std::fs::read_link(&link).unwrap_or_default(),
            directory: std::fs::metadata(&link).is_ok_and(|m| m.is_dir()),
        }
    });
    static REAL: OnceLock<usize> = OnceLock::new();
    let real = *REAL.get_or_init(|| next_in_line(c"fsync"));
    // SAFETY: `real` is libc's `fsync`, whose signature this is.
    unsafe { std::mem::transmute::<usize, unsafe extern "C" fn(c_int) -> c_int>(real)(fd) }
}

/// # Safety
/// `path` is a NUL-terminated string, as `unlink(2)` requires of its caller.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unlink(path: *const c_char) -> c_int {
    record(|| {
        // SAFETY: the caller's contract, above.
        let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
        Event::Unlink(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    });
    static REAL: OnceLock<usize> = OnceLock::new();
    let real = *REAL.get_or_init(|| next_in_line(c"unlink"));
    // SAFETY: `real` is libc's `unlink`, whose signature this is.
    unsafe {
        std::mem::transmute::<usize, unsafe extern "C" fn(*const c_char) -> c_int>(real)(path)
    }
}
