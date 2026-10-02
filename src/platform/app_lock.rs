//! Single-instance guard plus a "focus the running window" signal across launches.
//!
//! The first instance takes an OS lock and listens; a later launch detects it,
//! signals the running instance to raise its window, then exits before any shared
//! state (DB, the SDK credential LevelDB, Connect sockets, CEF) is touched. Windows
//! uses a SID-scoped named mutex + auto-reset event; Linux an abstract AF_UNIX
//! datagram socket (bind = lock, datagram = signal).

// Reaches only the focus-signal path, which exists on the platforms that have a
// guard to signal from.
#[cfg(any(windows, target_os = "linux"))]
use cef::*;

/// Raise the main window from any thread (posts to the CEF UI thread). No-op
/// until CEF is up.
#[cfg(any(windows, target_os = "linux"))]
fn post_focus() {
    if !crate::app_state::context_ready() {
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
            crate::ui::app_window::AppWindow::raise_current();
        }
    }
}

/// What became of a deep link a second launch tried to hand over.
///
/// One named value rather than a bare bool, because the site that reports has to
/// tell "no link was carried" from "a link was carried and did not land", and to
/// say which of the causes it was. Collapsing them is what left a lost click with
/// no trace at all and one message standing for two different worlds.
#[cfg(any(windows, target_os = "linux"))]
#[derive(Debug, PartialEq)]
enum HandOff {
    /// The running instance has the bytes.
    Landed,
    /// Longer than the channel carries, refused before anything was sent.
    TooLong,
    /// Nothing answered, and nothing appeared within the budget.
    NoListener,
    /// Something answered and stayed busy for the whole budget. Windows only: a
    /// datagram socket has no busy state to observe.
    #[cfg(windows)]
    Busy,
    /// The attempt itself broke: a write that failed, or a channel we could not
    /// even open.
    Failed,
}

#[cfg(any(windows, target_os = "linux"))]
impl HandOff {
    /// For the one message that names why a click went nowhere.
    fn reason(&self) -> &'static str {
        match self {
            Self::Landed => "delivered",
            Self::TooLong => "too long for the channel",
            Self::NoListener => "nothing was listening",
            #[cfg(windows)]
            Self::Busy => "the listener stayed busy",
            Self::Failed => "the channel failed",
        }
    }
}

/// What became of the pipe a clicked link travels on, for the instance that owns
/// the lock.
///
/// Windows only. Linux carries links on the lock socket itself, so it has no
/// second address to lose, and macOS never launches a second process at all.
#[cfg(windows)]
enum LinkChannel {
    /// A listener holds the address; a link arriving now reaches the window.
    Listening,
    /// Creating it failed, and nothing else will ever say why clicking a link
    /// stopped working.
    Unavailable,
    /// Never attempted, the guard having stood down for a reason it reported
    /// itself.
    NotAttempted,
}

/// Held for the process lifetime; dropping it releases the lock.
pub(crate) struct AppLock {
    #[cfg(windows)]
    _mutex: windows_impl::MutexGuard,
    #[cfg(windows)]
    link: LinkChannel,
    #[cfg(target_os = "linux")]
    _socket: Option<std::os::unix::net::UnixDatagram>,
    #[cfg(not(any(windows, target_os = "linux")))]
    _unsupported: (),
}

impl AppLock {
    /// Say why a click will go nowhere, once the session log is ours to write.
    ///
    /// The verdict waits on the type instead of leaving as a `verr!` where it is
    /// found. `verr!` opens the log file itself, and finding that file already
    /// open is what makes `adopt_session_log` stand its rotation down.
    pub(crate) fn report_link_channel(&self) {
        #[cfg(windows)]
        if matches!(self.link, LinkChannel::Unavailable) {
            crate::verr!("[LOCK]   no link pipe; deep links cannot reach this window");
        }
    }
}

/// `Some` if this is the first instance (keep the guard alive); `None` if another
/// instance is already running (it has been handed `url`, or a bare focus request
/// when there is none; exit).
pub(crate) fn acquire_or_signal(url: Option<&str>) -> Option<AppLock> {
    #[cfg(windows)]
    {
        windows_impl::acquire_or_signal(url)
    }
    #[cfg(target_os = "linux")]
    {
        linux_impl::acquire_or_signal(url)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        // macOS never launches a second process for a URL. LaunchServices
        // activates the running app and sends it an Apple Event instead.
        let _ = url;
        Some(AppLock { _unsupported: () })
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, FALSE, GENERIC_WRITE, GetLastError,
        HANDLE, INVALID_HANDLE_VALUE, LocalFree, WAIT_ABANDONED, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_INBOUND, ReadFile, WriteFile,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId,
        PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
        WaitNamedPipeW,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, GetCurrentProcess, INFINITE, OpenEventW,
        OpenProcessToken, SetEvent, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

    use super::{AppLock, HandOff, LinkChannel, post_focus};

    const LOCK_PREFIX: &str = "Global\\TidaLunarAppLock-";
    const FOCUS_PREFIX: &str = "Global\\TidaLunarAppFocus-";
    /// The event carries no data; a deep link needs its own channel. Named per
    /// SID like the other two, keeping one user's links out of another's session.
    const LINK_PIPE_PREFIX: &str = "\\\\.\\pipe\\TidaLunarAppLink-";

    /// Bounds one message. A longer write is refused rather than reassembled. The
    /// pipe is in message mode, and a deep link this size is not a deep link.
    const MAX_LINK: usize = 4096;

    /// How long a second launch keeps trying before it gives the link up.
    ///
    /// A single 200 ms wait used to stand here, on the reasoning that waiting
    /// longer would delay an exit without winning a race. That was wrong twice
    /// over: no OS bounds a protocol handler's lifetime -- the tightest ceiling
    /// anywhere is `xdg-open`'s 5 s portal timeout -- and 200 ms is less than the
    /// 250 ms Qt sleeps merely BETWEEN its two attempts at this same startup
    /// race, while Chromium spends up to 20 s. This sits between them.
    const LINK_HANDOFF_BUDGET_MS: u64 = 2000;

    /// One slice of the budget, letting an instance that frees up early be
    /// answered early instead of after the whole of it.
    const LINK_PIPE_WAIT_MS: u32 = 250;

    /// The pause before looking again for a pipe that is not there yet.
    /// `WaitNamedPipeW` needs an existing name, which leaves this the one case
    /// with nothing to wait on.
    const LINK_RETRY_PAUSE_MS: u64 = 50;

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

    // SAFETY: a Win32 HANDLE is process-wide, not thread-bound, and both kinds of
    // handle sent through here are used only by thread-safe APIs: the focus event
    // by WaitForSingleObject/SetEvent, the link pipe by ConnectNamedPipe, ReadFile,
    // DisconnectNamedPipe and CloseHandle. Using either from its listener thread is
    // sound, and each handle is owned by exactly one thread at a time.
    unsafe impl Send for SendHandle {}

    impl SendHandle {
        // Accessor (not a field read): the closure captures the whole wrapper;
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
            link: LinkChannel::NotAttempted,
        })
    }

    pub(super) fn acquire_or_signal(url: Option<&str>) -> Option<AppLock> {
        let Some(sid) = current_user_sid() else {
            crate::vprintln!("[LOCK]   SID lookup failed; single-instance guard disabled");
            return disabled();
        };
        let lock_name = wide(&format!("{LOCK_PREFIX}{sid}"));
        let focus_name = wide(&format!("{FOCUS_PREFIX}{sid}"));
        let pipe_name = wide(&format!("{LINK_PIPE_PREFIX}{sid}"));

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
            // The address exists before this function returns, letting a link
            // that arrives in the next instant find something to write to.
            // Created here rather than inside the listener thread, which the
            // caller outruns. A client arriving first got `ERROR_FILE_NOT_FOUND`,
            // and the only retry was gated on the pipe being busy instead.
            let link = match create_instance(&pipe_name) {
                Some(pipe) => {
                    spawn_link_listener(pipe_name, pipe);
                    LinkChannel::Listening
                }
                // Deep links are done for this run, and the window still comes
                // forward on a focus signal. The verdict travels rather than
                // being announced here; `report_link_channel` speaks it once the
                // log has been rotated.
                None => LinkChannel::Unavailable,
            };
            return Some(AppLock {
                _mutex: MutexGuard { handle: mutex },
                link,
            });
        }

        hand_off(url, &pipe_name, &focus_name);
        // SAFETY: release our handle, letting the kernel reap the object on owner exit.
        unsafe { CloseHandle(mutex) };
        None
    }

    /// Hands the running instance whatever this launch carried. A link that lands
    /// raises the window there; with no link, or one that did not land, the focus
    /// signal raises it.
    ///
    /// The pipe is opened for every launch, link or not: its server is the one
    /// process the kernel names, and this launch grants it the foreground. Windows
    /// refuses a raise from a signal to a process nobody granted (the window stays
    /// behind a flashing taskbar button); a launch started by the foreground
    /// process may grant it, as Chromium's process finder does.
    fn hand_off(url: Option<&str>, pipe_name: &[u16], focus_name: &[u16]) {
        let pipe = match connect_link_pipe(pipe_name) {
            Ok(pipe) => {
                grant_foreground(pipe);
                Ok(pipe)
            }
            Err(unanswered) => {
                crate::verr!("[LOCK]   no foreground grant: {}", unanswered.reason());
                Err(unanswered)
            }
        };
        let handoff = match (url, pipe) {
            (Some(raw), Ok(pipe)) if raw.len() <= MAX_LINK => Some(write_link(pipe, raw)),
            (url, Ok(pipe)) => {
                // No link, or one too long: closed unwritten, the far end's read
                // fails and delivers nothing.
                // SAFETY: our own client end, closed once.
                unsafe { CloseHandle(pipe) };
                url.map(|_| HandOff::TooLong)
            }
            (url, Err(unanswered)) => url.map(|_| unanswered),
        };
        match handoff {
            Some(HandOff::Landed) => {
                crate::vprintln!("[LOCK]   Handed a deep link to the running instance");
                return;
            }
            // Ungated, and this is the reason the channel has to be; this process
            // exits before the database opens and never learns the persisted
            // level, while a click answered by a window that did not navigate
            // leaves no other trace anywhere.
            Some(lost) => crate::verr!(
                "[LOCK]   deep link not delivered ({}); focusing instead",
                lost.reason()
            ),
            None => crate::vprintln!("[LOCK]   Another instance is running; focusing it"),
        }
        // SAFETY: focus_name is NUL-terminated UTF-16; null if no event exists yet.
        let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, FALSE, focus_name.as_ptr()) };
        if !event.is_null() {
            // SAFETY: event opened with EVENT_MODIFY_STATE; signalled then closed.
            unsafe {
                SetEvent(event);
                CloseHandle(event);
            }
        }
    }

    /// One connection at a time, serially. A deep link arrives on a click, not in
    /// a stream, and a queue here would only let a stalled reader hide the next one.
    fn spawn_link_listener(pipe_name: Vec<u16>, first: HANDLE) {
        let first = SendHandle(first);
        let _ = std::thread::Builder::new()
            .name("app-link-listener".to_owned())
            .spawn(move || {
                let mut pipe = first.get();
                loop {
                    // SAFETY: freshly created server end; blocks until a client connects.
                    unsafe { ConnectNamedPipe(pipe, ptr::null_mut()) };

                    // The next instance before this conversation ends, not after,
                    // since the name is the address clients look for and closing
                    // the only instance would take the address down with it.
                    let next = create_instance(&pipe_name);
                    serve_instance(pipe);
                    match next {
                        Some(handle) => pipe = handle,
                        None => return,
                    }
                }
            });
    }

    /// One server end of the link pipe, or `None` once the name is unusable.
    fn create_instance(pipe_name: &[u16]) -> Option<HANDLE> {
        // SAFETY: pipe_name is NUL-terminated UTF-16; returns an owned handle or
        // INVALID_HANDLE_VALUE.
        let pipe = unsafe {
            CreateNamedPipeW(
                pipe_name.as_ptr(),
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                0,
                MAX_LINK as u32,
                0,
                ptr::null(),
            )
        };
        if pipe == INVALID_HANDLE_VALUE {
            crate::vprintln!("[LOCK]   Link pipe unavailable: {}", unsafe {
                GetLastError()
            });
            return None;
        }
        Some(pipe)
    }

    /// The client end of the link pipe, or `None` when no free instance answers.
    fn open_instance(pipe_name: &[u16]) -> Option<HANDLE> {
        // SAFETY: pipe_name is NUL-terminated UTF-16; returns an owned handle or
        // INVALID_HANDLE_VALUE when no free server end exists.
        let pipe = unsafe {
            CreateFileW(
                pipe_name.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_NONE,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        };
        (pipe != INVALID_HANDLE_VALUE).then_some(pipe)
    }

    /// Reads one message off a connected instance, then closes it.
    fn serve_instance(pipe: HANDLE) {
        let mut buf = [0u8; MAX_LINK];
        let mut read: u32 = 0;
        // SAFETY: buf outlives the call and read receives the count.
        let ok = unsafe {
            ReadFile(
                pipe,
                buf.as_mut_ptr(),
                MAX_LINK as u32,
                &mut read,
                ptr::null_mut(),
            )
        };
        if ok != 0
            && let Ok(url) = std::str::from_utf8(&buf[..read as usize])
        {
            crate::ui::deep_link::deliver(url);
        }

        // SAFETY: our own server end; disconnect then close, once.
        unsafe {
            DisconnectNamedPipe(pipe);
            CloseHandle(pipe);
        }
    }

    /// Retries only while the OS says the answer could still change.
    ///
    /// The retry is bounded by diagnosis rather than by patience: a busy pipe and
    /// an absent one are both transient and get the budget, anything else is
    /// answered once and reported. The previous single attempt lost the startup
    /// race outright.
    fn connect_link_pipe(pipe_name: &[u16]) -> Result<HANDLE, HandOff> {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(LINK_HANDOFF_BUDGET_MS);
        loop {
            if let Some(pipe) = open_instance(pipe_name) {
                return Ok(pipe);
            }
            // Why this attempt found nothing, kept only for the iteration that
            // runs out of budget.
            // SAFETY: reads this thread's last error, just set by open_instance.
            let unanswered = match unsafe { GetLastError() } {
                // Every instance is serving someone. The listener creates the
                // next one while the current is still connected, freeing this as
                // soon as that conversation ends.
                ERROR_PIPE_BUSY => {
                    // SAFETY: pipe_name is NUL-terminated UTF-16.
                    unsafe { WaitNamedPipeW(pipe_name.as_ptr(), LINK_PIPE_WAIT_MS) };
                    HandOff::Busy
                }
                // The name is not there. Nothing to wait on, so look again.
                ERROR_FILE_NOT_FOUND => {
                    std::thread::sleep(std::time::Duration::from_millis(LINK_RETRY_PAUSE_MS));
                    HandOff::NoListener
                }
                // Not a race, since another attempt cannot answer differently.
                _ => return Err(HandOff::Failed),
            };
            if std::time::Instant::now() >= deadline {
                return Err(unanswered);
            }
        }
    }

    /// Lets the process serving `pipe` take the foreground until the user's next
    /// input elsewhere.
    ///
    /// The PID comes from the kernel, not the other end, and the grant reaches
    /// exactly the process a link written here would (`ASFW_ANY` would hand it to
    /// the whole session). Ungated for the reason the lost-link report is (see
    /// `hand_off`).
    fn grant_foreground(pipe: HANDLE) {
        let mut pid: u32 = 0;
        // SAFETY: a live client end of a named pipe; pid receives the server's id.
        if unsafe { GetNamedPipeServerProcessId(pipe, &mut pid) } == 0 {
            crate::verr!(
                "[LOCK]   no foreground grant: server pid unknown (gle={})",
                // SAFETY: reads this thread's last error, just set by the call above.
                unsafe { GetLastError() }
            );
            return;
        }
        // SAFETY: plain Win32 call on a process id; no memory is shared.
        let ok = unsafe { AllowSetForegroundWindow(pid) };
        if ok == 0 {
            crate::verr!(
                "[LOCK]   foreground grant to pid {pid} refused (gle={})",
                // SAFETY: reads this thread's last error, just set by the call above.
                unsafe { GetLastError() }
            );
        } else {
            crate::verr!("[LOCK]   foreground granted to pid {pid}");
        }
    }

    /// Writes one whole message into a connected client end, then closes it.
    fn write_link(pipe: HANDLE, url: &str) -> HandOff {
        let mut written: u32 = 0;
        // SAFETY: url outlives the call; written receives the count.
        let ok = unsafe {
            WriteFile(
                pipe,
                url.as_ptr(),
                url.len() as u32,
                &mut written,
                ptr::null_mut(),
            )
        };
        // SAFETY: our own client end, closed once.
        unsafe { CloseHandle(pipe) };
        if ok != 0 && written as usize == url.len() {
            HandOff::Landed
        } else {
            HandOff::Failed
        }
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
    use std::os::fd::AsRawFd;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{AppLock, HandOff, post_focus};

    /// Who sent a datagram, as the kernel reports it rather than as the payload
    /// claims.
    ///
    /// The address is no barrier: `unix(7)` is explicit that an abstract socket
    /// has no permission model, since neither the umask nor ownership affect its
    /// reachability, and the name is a hash of a path that is not a secret.
    /// Every process in the same network namespace can reach it, whoever owns it.
    /// Windows reaches the same boundary without asking, its named pipe's default
    /// descriptor granting writing to the creator's own account and only reading
    /// to everyone else.
    #[derive(Debug, PartialEq)]
    pub(super) enum Sender {
        /// Credentials the kernel vouched for, naming this process's own user.
        Ours,
        /// Another local user. Nothing here is theirs to drive.
        Foreign(u32),
        /// No credential arrived, leaving nobody to attribute it to.
        Unattributed,
    }

    /// Kept off the receive path so that a foreign sender can be exercised
    /// without one. A child forked in a test carries the test's own uid, and one
    /// account cannot produce another's.
    pub(super) fn classify(peer: Option<u32>, own: u32) -> Sender {
        match peer {
            Some(uid) if uid == own => Sender::Ours,
            Some(uid) => Sender::Foreign(uid),
            None => Sender::Unattributed,
        }
    }

    pub(super) fn own_uid() -> u32 {
        // SAFETY: reads the calling process's real uid and cannot fail.
        unsafe { libc::getuid() }
    }

    /// Asks the kernel to attach the sender's credentials to every datagram that
    /// arrives afterwards. Without it `recvmsg` carries no control message and
    /// there is nothing to check.
    pub(super) fn enable_peer_credentials(socket: &UnixDatagram) -> std::io::Result<()> {
        let on: libc::c_int = 1;
        // SAFETY: the option value is a `c_int` and the length passed is its own
        // size, which is the shape `SO_PASSCRED` expects. The descriptor is owned
        // by `socket`, which outlives the call.
        let rc = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PASSCRED,
                std::ptr::from_ref(&on).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// One datagram, plus the uid the kernel attributes it to.
    ///
    /// The credential travels as a control message, and `std` still gates its
    /// ancillary-data support behind nightly; the chain is walked here. The
    /// union is what aligns that buffer for a `cmsghdr`. An array of bytes
    /// promises no alignment, and its `align` arm exists for its type rather
    /// than its value.
    pub(super) fn recv_with_sender(
        socket: &UnixDatagram,
        buf: &mut [u8],
    ) -> std::io::Result<(usize, Option<u32>)> {
        #[repr(C)]
        union ControlBuffer {
            align: libc::cmsghdr,
            bytes: [u8; 64],
        }

        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        let mut control = ControlBuffer { bytes: [0; 64] };
        // SAFETY: `msghdr` is a C struct, and the fields left alone below are the
        // ones the kernel expects to read as zero.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = std::ptr::from_mut(&mut control).cast();
        msg.msg_controllen = std::mem::size_of::<ControlBuffer>() as _;

        // SAFETY: `msg` names the two buffers above, both of which outlive this
        // call, and the descriptor belongs to `socket`.
        let received = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut msg, 0) };
        if received < 0 {
            return Err(std::io::Error::last_os_error());
        }

        let mut peer = None;
        // SAFETY: the receive succeeded, so `msg`'s control fields describe what
        // the kernel wrote into our own buffer, and the two macros only ever
        // yield a pointer inside it, aligned for a header by the union above.
        // Each read is unaligned so that neither claims more than that.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                let header = std::ptr::read_unaligned(cmsg);
                if header.cmsg_level == libc::SOL_SOCKET
                    && header.cmsg_type == libc::SCM_CREDENTIALS
                {
                    let cred =
                        std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::ucred>());
                    peer = Some(cred.uid);
                    break;
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }

        Ok((received as usize, peer))
    }

    /// What a second launch sends when it carries no URL. One socket carries both
    /// meanings here, unlike Windows where the deep link has a channel of its own
    /// and the focus signal keeps its data-less event.
    pub(super) const FOCUS_SIGNAL: &str = "focus";

    /// Sized for a deep link rather than the 5-byte focus word. A datagram longer
    /// than this is truncated by the kernel, and the truncated remainder fails the
    /// URL rule rather than navigating somewhere unintended.
    pub(super) const MAX_SIGNAL: usize = 4096;

    /// Per-user abstract socket address (data-dir-scoped; auto-reclaimed on exit).
    fn lock_addr() -> std::io::Result<SocketAddr> {
        let data_dir = crate::state::cache_data_dir();
        let mut hasher = fnv::FnvHasher::default();
        hasher.write(data_dir.to_string_lossy().as_bytes());
        SocketAddr::from_abstract_name(format!("tidalunar-app-{:016x}", hasher.finish()))
    }

    pub(super) fn acquire_or_signal(url: Option<&str>) -> Option<AppLock> {
        let Ok(addr) = lock_addr() else {
            crate::vprintln!("[LOCK]   Could not build lock address; guard disabled");
            return Some(AppLock { _socket: None });
        };

        match UnixDatagram::bind_addr(&addr) {
            Ok(socket) => {
                // Failing here is not fatal and not ignored either. With no
                // credentials arriving, every datagram is unattributed, and an
                // unattributed one is refused. The channel closes rather than
                // carrying a payload it cannot attribute.
                if let Err(e) = enable_peer_credentials(&socket) {
                    crate::vprintln!("[LOCK]   no peer credentials ({e}); signals will be refused");
                }
                if let Ok(listener) = socket.try_clone() {
                    spawn_focus_listener(listener);
                }
                Some(AppLock {
                    _socket: Some(socket),
                })
            }
            Err(e) if e.kind() == ErrorKind::AddrInUse => {
                let outcome = hand_over(&addr, url);
                match (url, &outcome) {
                    (Some(_), HandOff::Landed) => {
                        crate::vprintln!("[LOCK]   Handed a deep link to the running instance");
                    }
                    // Ungated, and this is why the channel has to be; this
                    // process exits before the database opens and never learns
                    // the persisted level, while a click answered by a window
                    // that did not navigate leaves no other trace.
                    (Some(_), lost) => crate::verr!(
                        "[LOCK]   deep link not delivered ({}); focusing instead",
                        lost.reason()
                    ),
                    (None, HandOff::Landed) => {
                        crate::vprintln!("[LOCK]   Another instance is running; focusing it");
                    }
                    (None, lost) => crate::verr!(
                        "[LOCK]   could not reach the running instance ({})",
                        lost.reason()
                    ),
                }
                None
            }
            Err(e) => {
                crate::vprintln!("[LOCK]   bind failed ({e}); guard disabled");
                Some(AppLock { _socket: None })
            }
        }
    }

    /// Hands over whatever this launch carried, falling back to the bare focus
    /// word so a click still brings the window forward.
    ///
    /// One socket carries both meanings here, unlike Windows where the focus
    /// signal keeps an event of its own. Failing to open or connect it loses the
    /// focus too, and that is the one case nothing here can rescue. It is
    /// reported instead.
    pub(super) fn hand_over(addr: &SocketAddr, url: Option<&str>) -> HandOff {
        let Ok(client) = UnixDatagram::unbound() else {
            return HandOff::Failed;
        };
        if client.connect_addr(addr).is_err() {
            return HandOff::NoListener;
        }
        let Some(url) = url else {
            return send_or_fail(&client, FOCUS_SIGNAL);
        };
        // Refused here rather than truncated by the kernel on arrival, since a
        // cut URL fails the route rule at the far end and reads as a refused
        // link instead of an oversized one.
        if url.len() > MAX_SIGNAL {
            let _ = client.send(FOCUS_SIGNAL.as_bytes());
            return HandOff::TooLong;
        }
        if client.send(url.as_bytes()).is_ok() {
            return HandOff::Landed;
        }
        // The link did not go; the window still should.
        let _ = client.send(FOCUS_SIGNAL.as_bytes());
        HandOff::Failed
    }

    fn send_or_fail(client: &UnixDatagram, word: &str) -> HandOff {
        if client.send(word.as_bytes()).is_ok() {
            HandOff::Landed
        } else {
            HandOff::Failed
        }
    }

    fn spawn_focus_listener(socket: UnixDatagram) {
        let _ = std::thread::Builder::new()
            .name("app-focus-listener".to_owned())
            .spawn(move || {
                let own = own_uid();
                let mut buf = [0u8; MAX_SIGNAL];
                // The length matters now that the payload varies (a receive that
                // ignored it would read stale bytes from the previous datagram).
                while let Ok((n, peer)) = recv_with_sender(&socket, &mut buf) {
                    match classify(peer, own) {
                        Sender::Ours => match std::str::from_utf8(&buf[..n]) {
                            Ok(FOCUS_SIGNAL) => post_focus(),
                            Ok(url) => crate::ui::deep_link::deliver(url),
                            Err(_) => post_focus(),
                        },
                        Sender::Foreign(uid) => refuse(Some(uid)),
                        Sender::Unattributed => refuse(None),
                    }
                }
            });
    }

    /// Said once per run. A local process can repeat a refused datagram as fast
    /// as it likes, and a log it can fill is a second defect.
    fn refuse(uid: Option<u32>) {
        static ANNOUNCED: AtomicBool = AtomicBool::new(false);
        if ANNOUNCED.swap(true, Ordering::Relaxed) {
            return;
        }
        match uid {
            Some(uid) => crate::vprintln!("[LOCK]   refused a signal from uid {uid}"),
            None => crate::vprintln!("[LOCK]   refused a signal nobody is named for"),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "../../tests/unit/platform/app_lock.rs"]
mod tests;
