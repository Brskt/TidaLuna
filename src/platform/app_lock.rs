//! Single-instance guard plus a "focus the running window" signal across launches.
//!
//! The first instance takes an OS lock and listens; a later launch detects it,
//! signals the running instance to raise its window, then exits before any shared
//! state (DB, the SDK credential LevelDB, Connect sockets, CEF) is touched. Windows
//! uses a SID-scoped named mutex + auto-reset event; Linux an abstract AF_UNIX
//! datagram socket (bind = lock, datagram = signal).

use std::sync::atomic::{AtomicBool, Ordering};

use cef::*;

/// Gates `post_focus` until the CEF UI thread exists.
static CONTEXT_READY: AtomicBool = AtomicBool::new(false);

/// Let the focus listener post UI tasks; called once CEF is initialized.
pub(crate) fn mark_context_ready() {
    CONTEXT_READY.store(true, Ordering::Release);
}

/// Raise the main window from any thread (posts to the CEF UI thread). No-op
/// until CEF is up.
#[cfg(any(windows, target_os = "linux"))]
fn post_focus() {
    if !CONTEXT_READY.load(Ordering::Acquire) {
        return;
    }
    let mut task = FocusWindowTask::new(0);
    post_task(ThreadId::UI, Some(&mut task));
}

#[cfg(any(windows, target_os = "linux"))]
wrap_task! {
    struct FocusWindowTask {
        _p: u8,
    }
    impl Task {
        fn execute(&self) {
            if let Some(window) = crate::ui::app_window::AppWindow::current() {
                window.restore();
                window.show();
                window.focus_foreground();
            }
        }
    }
}

/// Held for the process lifetime; dropping it releases the lock.
pub(crate) struct AppLock {
    #[cfg(windows)]
    _mutex: windows_impl::MutexGuard,
    #[cfg(target_os = "linux")]
    _socket: Option<std::os::unix::net::UnixDatagram>,
    #[cfg(not(any(windows, target_os = "linux")))]
    _unsupported: (),
}

/// `Some` if this is the first instance (keep the guard alive); `None` if another
/// instance is already running (it has been signalled to focus, so exit).
pub(crate) fn acquire_or_signal() -> Option<AppLock> {
    #[cfg(windows)]
    {
        windows_impl::acquire_or_signal()
    }
    #[cfg(target_os = "linux")]
    {
        linux_impl::acquire_or_signal()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Some(AppLock { _unsupported: () })
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        CloseHandle, FALSE, GetLastError, HANDLE, LocalFree, WAIT_ABANDONED, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, GetCurrentProcess, INFINITE, OpenEventW,
        OpenProcessToken, SetEvent, WaitForSingleObject,
    };

    use super::{AppLock, post_focus};

    const LOCK_PREFIX: &str = "Global\\TidaLunarAppLock-";
    const FOCUS_PREFIX: &str = "Global\\TidaLunarAppFocus-";

    /// Releases the mutex on drop; a null handle (fail-open) is a no-op.
    pub(super) struct MutexGuard {
        handle: HANDLE,
    }

    impl Drop for MutexGuard {
        fn drop(&mut self) {
            // SAFETY: handle is null (no-op) or a live CreateMutexW handle we close once.
            unsafe {
                if !self.handle.is_null() {
                    CloseHandle(self.handle);
                }
            }
        }
    }

    struct SendHandle(HANDLE);

    // SAFETY: a Win32 HANDLE is process-wide and the event APIs (WaitForSingleObject,
    // SetEvent) are thread-safe, so using it from the listener thread is sound.
    unsafe impl Send for SendHandle {}

    impl SendHandle {
        // Accessor (not a field read) so the closure captures the whole wrapper -
        // disjoint capture of `.0` alone would move a bare `*mut c_void`, not Send.
        fn get(&self) -> HANDLE {
            self.0
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Per-user SID string, used to scope the lock; `None` on failure (fail open).
    fn current_user_sid() -> Option<String> {
        // SAFETY: token-query sequence on our own process; every handle and the SID
        // string allocation is released on each return path.
        unsafe {
            let mut token: HANDLE = ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return None;
            }

            let mut needed: u32 = 0;
            // Sizing call: returns FALSE, we only want `needed`.
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed);
            if needed == 0 {
                CloseHandle(token);
                return None;
            }

            let mut buf = vec![0u8; needed as usize];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr().cast(),
                needed,
                &mut needed,
            );
            CloseHandle(token);
            if ok == 0 {
                return None;
            }

            let token_user = buf.as_ptr().cast::<TOKEN_USER>();
            let sid = (*token_user).User.Sid;

            let mut sid_str: *mut u16 = ptr::null_mut();
            if ConvertSidToStringSidW(sid, &mut sid_str) == 0 {
                return None;
            }

            let mut len = 0usize;
            while *sid_str.add(len) != 0 {
                len += 1;
            }
            let s = OsString::from_wide(std::slice::from_raw_parts(sid_str, len))
                .to_string_lossy()
                .into_owned();
            LocalFree(sid_str.cast());
            Some(s)
        }
    }

    fn disabled() -> Option<AppLock> {
        Some(AppLock {
            _mutex: MutexGuard {
                handle: ptr::null_mut(),
            },
        })
    }

    pub(super) fn acquire_or_signal() -> Option<AppLock> {
        let Some(sid) = current_user_sid() else {
            crate::vprintln!("[LOCK]   SID lookup failed; single-instance guard disabled");
            return disabled();
        };
        let lock_name = wide(&format!("{LOCK_PREFIX}{sid}"));
        let focus_name = wide(&format!("{FOCUS_PREFIX}{sid}"));

        // SAFETY: lock_name is NUL-terminated UTF-16; returns an owned handle or null.
        let mutex = unsafe { CreateMutexW(ptr::null(), FALSE, lock_name.as_ptr()) };
        if mutex.is_null() {
            crate::vprintln!("[LOCK]   CreateMutexW failed: {}", unsafe {
                GetLastError()
            });
            return disabled();
        }

        // SAFETY: mutex is a live handle; the wait is thread-safe.
        let wait = unsafe { WaitForSingleObject(mutex, 0) };
        if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
            // First instance: own the lock and listen for focus signals.
            // SAFETY: focus_name is NUL-terminated UTF-16; auto-reset, owned handle.
            let event = unsafe { CreateEventW(ptr::null(), FALSE, FALSE, focus_name.as_ptr()) };
            if !event.is_null() {
                spawn_focus_listener(event);
            }
            return Some(AppLock {
                _mutex: MutexGuard { handle: mutex },
            });
        }

        // Another instance is running: signal it to focus, then exit.
        crate::vprintln!("[LOCK]   Another instance is running; focusing it");
        // SAFETY: focus_name is NUL-terminated UTF-16; null if no event exists yet.
        let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, FALSE, focus_name.as_ptr()) };
        if !event.is_null() {
            // SAFETY: event opened with EVENT_MODIFY_STATE; signalled then closed.
            unsafe {
                SetEvent(event);
                CloseHandle(event);
            }
        }
        // SAFETY: release our handle so the kernel reaps the object on owner exit.
        unsafe { CloseHandle(mutex) };
        None
    }

    fn spawn_focus_listener(event: HANDLE) {
        let event = SendHandle(event);
        let _ = std::thread::Builder::new()
            .name("app-focus-listener".to_owned())
            .spawn(move || {
                loop {
                    // SAFETY: live auto-reset event for the process lifetime; thread-safe wait.
                    let r = unsafe { WaitForSingleObject(event.get(), INFINITE) };
                    if r != WAIT_OBJECT_0 {
                        break;
                    }
                    post_focus();
                }
            });
    }
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::hash::Hasher;
    use std::io::ErrorKind;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};

    use super::{AppLock, post_focus};

    /// Per-user abstract socket address (data-dir-scoped; auto-reclaimed on exit).
    fn lock_addr() -> std::io::Result<SocketAddr> {
        let data_dir = crate::state::cache_data_dir();
        let mut hasher = fnv::FnvHasher::default();
        hasher.write(data_dir.to_string_lossy().as_bytes());
        SocketAddr::from_abstract_name(format!("tidalunar-app-{:016x}", hasher.finish()))
    }

    pub(super) fn acquire_or_signal() -> Option<AppLock> {
        let Ok(addr) = lock_addr() else {
            crate::vprintln!("[LOCK]   Could not build lock address; guard disabled");
            return Some(AppLock { _socket: None });
        };

        match UnixDatagram::bind_addr(&addr) {
            Ok(socket) => {
                if let Ok(listener) = socket.try_clone() {
                    spawn_focus_listener(listener);
                }
                Some(AppLock {
                    _socket: Some(socket),
                })
            }
            Err(e) if e.kind() == ErrorKind::AddrInUse => {
                if let Ok(client) = UnixDatagram::unbound()
                    && client.connect_addr(&addr).is_ok()
                {
                    let _ = client.send(b"focus");
                }
                None
            }
            Err(e) => {
                crate::vprintln!("[LOCK]   bind failed ({e}); guard disabled");
                Some(AppLock { _socket: None })
            }
        }
    }

    fn spawn_focus_listener(socket: UnixDatagram) {
        let _ = std::thread::Builder::new()
            .name("app-focus-listener".to_owned())
            .spawn(move || {
                let mut buf = [0u8; 16];
                while socket.recv(&mut buf).is_ok() {
                    post_focus();
                }
            });
    }
}
