//! SIGTERM, SIGINT and SIGHUP as quit requests.
//!
//! Chromium installs its own handlers for these during `initialize`, and on SIGTERM it ends
//! the process without `main` returning. Ours replace them rather than chain: a chained
//! Chromium handler would still race our exit to `_exit(0)`.

use std::io::Read;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};

use libc::c_int;

const SIGNALS: [c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// Write end of the pipe the handler reports through; -1 until the reader is started.
static REPORT_FD: AtomicI32 = AtomicI32::new(-1);

/// Which of [`SIGNALS`] the parent left ignored: `nohup` does it for SIGHUP, a script's `&`
/// for SIGINT. They stay ignored, as POSIX keeps them for a non-interactive shell.
static IGNORED_ON_ENTRY: OnceLock<[bool; 3]> = OnceLock::new();

fn ignored_now() -> [bool; 3] {
    SIGNALS.map(|sig| {
        // SAFETY: a null new action only reads the current one.
        unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            libc::sigaction(sig, std::ptr::null(), &mut current) == 0
                && current.sa_sigaction == libc::SIG_IGN
        }
    })
}

/// Points the three signals at our handler, over whatever handles them now, or back to
/// ignored for those the parent left ignored. Called once the database is open, and again after
/// `initialize`, which resets them and lets Chromium in; the first call reads what the parent left.
pub(crate) fn install() {
    let ignored = IGNORED_ON_ENTRY.get_or_init(ignored_now);
    if REPORT_FD.load(Ordering::Acquire) < 0
        && let Err(e) = start_reader()
    {
        crate::verr!("[QUIT]   Quit signals keep their current handlers ({e})");
        return;
    }
    for (sig, ignored) in SIGNALS.into_iter().zip(ignored.iter().copied()) {
        // SAFETY: a zeroed `sigaction` is a valid empty one; the handler is async-signal-safe.
        let failed = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = if ignored {
                libc::SIG_IGN
            } else {
                on_signal as extern "C" fn(c_int) as libc::sighandler_t
            };
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(sig, &action, std::ptr::null_mut()) != 0
        };
        if failed {
            let e = std::io::Error::last_os_error();
            crate::verr!("[QUIT]   Signal {sig} keeps its current handler ({e})");
        }
    }
}

fn start_reader() -> std::io::Result<()> {
    let (mut reader, writer) = std::io::pipe()?;
    std::thread::Builder::new()
        .name("quit-signal".into())
        .spawn(move || {
            let mut byte = [0u8; 1];
            loop {
                match reader.read(&mut byte) {
                    Ok(1) => {
                        super::request(super::End::Signal(c_int::from(byte[0])), super::GRACE);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    _ => return,
                }
            }
        })?;
    REPORT_FD.store(
        std::os::fd::IntoRawFd::into_raw_fd(writer),
        Ordering::Release,
    );
    Ok(())
}

/// Async-signal-safe only: an atomic store and one byte down the pipe. SIGINT and SIGTERM get
/// one shot (the default action comes back: a second one ends the process at once). SIGHUP
/// keeps the handler, as a closing terminal sends it twice (the shell, then the kernel).
extern "C" fn on_signal(sig: c_int) {
    // Here rather than in the reader: a renderer killed by the same group signal must not be
    // reported as a crash before `quit-signal` wakes.
    super::REQUESTED.store(true, Ordering::Release);
    // SAFETY: `sigaction`, `write` and errno access are async-signal-safe.
    unsafe {
        let errno = errno_location();
        let saved = *errno;
        if sig != libc::SIGHUP {
            let mut default: libc::sigaction = std::mem::zeroed();
            default.sa_sigaction = libc::SIG_DFL;
            libc::sigaction(sig, &default, std::ptr::null_mut());
        }
        let byte = sig as u8;
        let fd = REPORT_FD.load(Ordering::Acquire);
        while libc::write(fd, (&raw const byte).cast(), 1) < 0 && *errno == libc::EINTR {}
        *errno = saved;
    }
}

#[cfg(target_os = "linux")]
unsafe fn errno_location() -> *mut c_int {
    // SAFETY: always valid for the calling thread.
    unsafe { libc::__errno_location() }
}

#[cfg(target_os = "macos")]
unsafe fn errno_location() -> *mut c_int {
    // SAFETY: always valid for the calling thread.
    unsafe { libc::__error() }
}

// The tests read thread state through procfs.
#[cfg(all(test, target_os = "linux"))]
#[path = "../../../tests/unit/platform/quit/signal.rs"]
mod tests;
