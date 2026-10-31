//! Single-instance guard plus a "focus the running window" signal across launches.
//!
//! The first instance takes an OS lock and listens; a later launch detects it,
//! signals the running instance to raise its window, then exits before any shared
//! state (DB, the SDK credential LevelDB, Connect sockets, CEF) is touched. One
//! that finds the instance closing waits for it to leave and takes over. Windows
//! uses a named mutex + auto-reset event in a private namespace bounded by the
//! user's SID; Linux an flock on a file in the data directory, with an AF_UNIX
//! datagram socket in a private directory as the signal.

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
    /// The name answered, but another account holds it. Nothing was granted or
    /// written.
    #[cfg(windows)]
    ForeignServer,
    /// The name answered, but its owner could not be read, leaving nobody to vouch
    /// for the server.
    #[cfg(windows)]
    UnattributedServer,
    /// The running instance turned the channel away on purpose: it is on its way
    /// out and still holds the lock until it is gone. Linux only: a pipe has no
    /// way to say so, and its absence is all a client on Windows finds.
    #[cfg(target_os = "linux")]
    Closing,
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
            #[cfg(windows)]
            Self::ForeignServer => "another account holds the channel",
            #[cfg(windows)]
            Self::UnattributedServer => "the channel's owner could not be read",
            #[cfg(target_os = "linux")]
            Self::Closing => "the running instance is closing",
            Self::Failed => "the channel failed",
        }
    }
}

/// How long a second launch keeps trying before it gives the launch up: to reach
/// the channel, and to outwait a running instance that is leaving.
///
/// One deadline for both, started when the lock is found taken. The tightest
/// ceiling on a protocol handler is `xdg-open`'s 5 s portal timeout; Firefox
/// retries its profile lock for 5 s and Chromium on Windows its lockfile for 5 s,
/// for the same instance still closing down.
#[cfg(any(windows, target_os = "linux"))]
const HANDOFF_BUDGET_MS: u64 = 5000;

/// How long a launch looks for a channel that is not there, from a holder that is
/// not leaving, before reporting it. A Linux holder binds right after taking the
/// lock (45 ms at worst over 3000 trials with twice as many busy threads as cores);
/// an absence past that is no race.
#[cfg(any(windows, target_os = "linux"))]
const NO_LISTENER_GRACE_MS: u64 = 250;

/// How long closing waits for a listener to stop.
#[cfg(any(windows, target_os = "linux"))]
const CLOSE_WAIT_MS: u64 = 500;

/// Waits for a listener thread, but no longer than `budget`: a listener held by a
/// peer that never finishes writing must not hold the quit behind it.
#[cfg(any(windows, target_os = "linux"))]
fn join_within(listener: std::thread::JoinHandle<()>, budget: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    while !listener.is_finished() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let _ = listener.join();
    true
}

/// Turns the channel away for the rest of the process: a link or focus request sent after
/// this fails visibly, and its sender waits for the lock. Called where the message loop is
/// told to quit and where a launch ends after taking the lock (it outlives the service by
/// the whole teardown). Idempotent; a no-op before the lock is taken and on macOS.
pub(crate) fn close_channel() {
    #[cfg(windows)]
    windows_impl::close_channel();
    #[cfg(target_os = "linux")]
    linux_impl::close_channel();
}

/// Whose kernel object this is, the link pipe or the legacy lock, by the owner the
/// kernel stamped on it. A name is a first-come claim any account can make; the
/// owner is the one fact its creator cannot forge.
#[cfg(any(windows, test))]
#[derive(Debug, PartialEq)]
enum ObjectOwner {
    Ours,
    /// Another account holds the name, kept for the log line.
    Foreign(String),
    /// The owner could not be read.
    Unattributed,
}

/// The owners our own token stamps on what it creates: its user, and its default
/// owner, which is BUILTIN\Administrators when the token is elevated.
#[cfg(any(windows, test))]
struct OwnIdentity {
    user: String,
    default_owner: String,
}

/// Exact equality on both sides' `ConvertSidToStringSidW` form: a SID that merely
/// starts like ours is another account.
#[cfg(any(windows, test))]
fn classify_owner(owner: Option<&str>, own: &OwnIdentity) -> ObjectOwner {
    match owner {
        Some(sid) if sid == own.user || sid == own.default_owner => ObjectOwner::Ours,
        Some(sid) => ObjectOwner::Foreign(sid.to_owned()),
        None => ObjectOwner::Unattributed,
    }
}

/// What became of the pipe a clicked link travels on, for the instance that owns
/// the lock.
///
/// Windows only. Linux reports its own channel's absence where it finds it, and
/// macOS never launches a second process at all.
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
    #[cfg(windows)]
    closer: Option<std::sync::Arc<windows_impl::Closer>>,
    #[cfg(target_os = "linux")]
    _guard: Option<linux_impl::Guard>,
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_SUCCESS,
        FALSE, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree, TRUE,
        WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT, WIN32_ERROR,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SECURITY_ATTRIBUTES, TOKEN_INFORMATION_CLASS, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER,
        TokenOwner, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_INBOUND, ReadFile,
        SECURITY_ANONYMOUS, SECURITY_SQOS_PRESENT, WriteFile,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId,
        PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
        WaitNamedPipeW,
    };
    use windows_sys::Win32::System::Threading::{
        AddSIDToBoundaryDescriptor, ClosePrivateNamespace, CreateBoundaryDescriptorW, CreateEventW,
        CreateMutexW, CreatePrivateNamespaceW, DeleteBoundaryDescriptor, EVENT_MODIFY_STATE,
        GetCurrentProcess, INFINITE, OpenEventW, OpenPrivateNamespaceW, OpenProcessToken, SetEvent,
        WaitForMultipleObjects, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

    use super::{
        AppLock, CLOSE_WAIT_MS, HANDOFF_BUDGET_MS, HandOff, LinkChannel, NO_LISTENER_GRACE_MS,
        ObjectOwner, OwnIdentity, classify_owner, join_within, post_focus,
    };

    /// The namespace the lock and the focus event live in, named by its boundary:
    /// an alias is only this process's prefix for it, and two boundaries of one
    /// name and SID are one namespace.
    const NAMESPACE: &str = "TidaLunarApp";
    pub(super) const LOCK_LEAF: &str = "Lock";
    const FOCUS_LEAF: &str = "Focus";
    /// Names earlier builds use. Any account can claim a `Global\` name, which
    /// rules them out as the lock. Held anyway for an earlier build launched later
    /// to find; probed to avoid starting beside one that runs.
    const LEGACY_LOCK_PREFIX: &str = "Global\\TidaLunarAppLock-";
    const LEGACY_FOCUS_PREFIX: &str = "Global\\TidaLunarAppFocus-";
    /// The event carries no data; a deep link needs its own channel. Named per
    /// SID like the other two, keeping one user's links out of another's session.
    const LINK_PIPE_PREFIX: &str = "\\\\.\\pipe\\TidaLunarAppLink-";

    /// Bounds one message. A longer write is refused rather than reassembled. The
    /// pipe is in message mode, and a deep link this size is not a deep link.
    const MAX_LINK: usize = 4096;

    /// One slice of the budget, letting an instance that frees up early be
    /// answered early instead of after the whole of it.
    const LINK_PIPE_WAIT_MS: u32 = 250;

    /// The pause before looking again for a pipe that is not there yet.
    /// `WaitNamedPipeW` needs an existing name, which leaves this the one case
    /// with nothing to wait on but the lock, when there is one.
    const LINK_RETRY_PAUSE_MS: u32 = 50;

    /// Releases the lock on drop; a null handle (fail-open) is a no-op. Also holds
    /// the legacy mutex and the namespace, which drops after `drop` closes both.
    pub(super) struct MutexGuard {
        handle: HANDLE,
        legacy: HANDLE,
        _namespace: Option<UserNamespace>,
    }

    impl Drop for MutexGuard {
        fn drop(&mut self) {
            // SAFETY: each handle is null (no-op) or a live CreateMutexW handle we
            // close once.
            unsafe {
                for handle in [self.handle, self.legacy] {
                    if !handle.is_null() {
                        CloseHandle(handle);
                    }
                }
            }
        }
    }

    /// A private namespace bounded by this account's SID, open while this value
    /// lives. No other account can create it (its token lacks the SID) nor open it
    /// (the descriptor grants only this account and SYSTEM). One per process: a
    /// process that holds it cannot open it again (`ERROR_DUP_NAME`).
    pub(super) struct UserNamespace {
        handle: HANDLE,
        descriptor: PSECURITY_DESCRIPTOR,
    }

    impl UserNamespace {
        /// Create-or-open. The kernel refuses a boundary naming a SID absent from
        /// our token. The descriptor grants the same `sid`, so the two cannot
        /// disagree. The error is the failing call's, read before any clean-up.
        pub(super) fn open(name: &str, sid: PSID) -> Result<Self, WIN32_ERROR> {
            // SAFETY: reads this thread's last error, each call site right after
            // the call that failed.
            let last_error = || unsafe { GetLastError() };
            let sid_text = sid_string(sid).ok_or_else(last_error)?;
            let sddl = wide(&format!("D:P(A;;GA;;;{sid_text})(A;;GA;;;SY)"));
            let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
            // SAFETY: NUL-terminated SDDL; the call allocates the descriptor, which
            // Drop or the failure path below frees.
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            } == 0
            {
                return Err(last_error());
            }
            let name = wide(name);
            // SAFETY: NUL-terminated name; the boundary is deleted before returning.
            let mut boundary = unsafe { CreateBoundaryDescriptorW(name.as_ptr(), 0) };
            let opened = if boundary.is_null() {
                Err(last_error())
            } else {
                // SAFETY: a live boundary; `sid` points into a buffer the caller
                // keeps alive.
                let bounded = unsafe { AddSIDToBoundaryDescriptor(&mut boundary, sid) } != 0;
                let handle = if bounded {
                    create_or_open(&attributes_of(descriptor), boundary, &name)
                } else {
                    ptr::null_mut()
                };
                let opened = if handle.is_null() {
                    Err(last_error())
                } else {
                    Ok(handle)
                };
                // SAFETY: the boundary created above, deleted once.
                unsafe { DeleteBoundaryDescriptor(boundary) };
                opened
            };
            opened
                .map(|handle| Self { handle, descriptor })
                .inspect_err(|_| {
                    // SAFETY: the descriptor converted above, freed once.
                    unsafe { LocalFree(descriptor.cast()) };
                })
        }

        /// What every object in the namespace is created with, letting the
        /// limited and elevated halves of one account open what either made.
        pub(super) fn attributes(&self) -> SECURITY_ATTRIBUTES {
            attributes_of(self.descriptor)
        }
    }

    impl Drop for UserNamespace {
        fn drop(&mut self) {
            // SAFETY: our handle, closed once and not destroyed, so another
            // holder's objects stay reachable; then the descriptor we allocated.
            unsafe {
                ClosePrivateNamespace(self.handle, 0);
                LocalFree(self.descriptor.cast());
            }
        }
    }

    fn attributes_of(descriptor: PSECURITY_DESCRIPTOR) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: FALSE,
        }
    }

    /// One retry covers a namespace its last holder closed between our create and
    /// our open.
    fn create_or_open(attributes: &SECURITY_ATTRIBUTES, boundary: HANDLE, name: &[u16]) -> HANDLE {
        for _ in 0..2 {
            // SAFETY: the attributes, boundary and name outlive the call.
            let created = unsafe { CreatePrivateNamespaceW(attributes, boundary, name.as_ptr()) };
            // SAFETY: reads this thread's last error, set by the call above.
            if !created.is_null() || unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
                return created;
            }
            // SAFETY: as above.
            let opened = unsafe { OpenPrivateNamespaceW(boundary, name.as_ptr()) };
            if !opened.is_null() {
                return opened;
            }
        }
        ptr::null_mut()
    }

    struct SendHandle(HANDLE);

    // SAFETY: a Win32 HANDLE is process-wide, not thread-bound, and each kind of
    // handle sent through here is used only by thread-safe APIs: the focus event
    // by WaitForMultipleObjects/SetEvent, the link pipe by ConnectNamedPipe,
    // ReadFile, DisconnectNamedPipe and CloseHandle, the stop event by SetEvent and
    // the same wait. A focus event and a pipe belong to exactly one thread at a time;
    // the stop event is shared, and is only signalled, never closed.
    unsafe impl Send for SendHandle {}
    // SAFETY: shared only through the stop event, whose two uses are thread-safe.
    unsafe impl Sync for SendHandle {}

    impl SendHandle {
        // Accessor (not a field read): the closure captures the whole wrapper;
        // disjoint capture of `.0` alone would move a bare `*mut c_void`, not Send.
        fn get(&self) -> HANDLE {
            self.0
        }
    }

    pub(super) fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// `leaf` inside the namespace `ns`, which `UserNamespace::open` registers
    /// under its own name as the alias.
    pub(super) fn in_namespace(ns: &str, leaf: &str) -> Vec<u16> {
        wide(&format!("{ns}\\{leaf}"))
    }

    /// A SID in its string form, the one both sides of an owner check compare.
    fn sid_string(sid: PSID) -> Option<String> {
        // SAFETY: sid points into a buffer the caller keeps alive; the string
        // allocation is released before returning.
        unsafe {
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

    /// One token information class, in a buffer sized by the sizing call.
    fn token_info(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Option<Vec<u8>> {
        let mut needed: u32 = 0;
        // SAFETY: a live token handle; the sizing call returns FALSE, we only want `needed`.
        unsafe { GetTokenInformation(token, class, ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            return None;
        }
        let mut buf = vec![0u8; needed as usize];
        // SAFETY: buf holds `needed` bytes and outlives the call.
        let ok = unsafe {
            GetTokenInformation(token, class, buf.as_mut_ptr().cast(), needed, &mut needed)
        };
        (ok != 0).then_some(buf)
    }

    /// Our token, queried once: its user, whose SID bounds the namespace and scopes
    /// every name here, and the string forms of that user and of the owner the token
    /// stamps on what it creates. `None` on failure (fail open).
    pub(super) fn own_identity() -> Option<(OwnIdentity, OwnUser)> {
        let mut token: HANDLE = ptr::null_mut();
        // SAFETY: our own process token, queried then closed once.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return None;
        }
        let user = token_info(token, TokenUser);
        let owner = token_info(token, TokenOwner);
        // SAFETY: the token handle opened above, closed once.
        unsafe { CloseHandle(token) };
        let user = OwnUser(user?);
        // SAFETY: a buffer GetTokenInformation(TokenOwner) filled, alive until the
        // conversion returns. Read unaligned: a `Vec<u8>` promises no more.
        let default_owner = sid_string(unsafe {
            ptr::read_unaligned(owner?.as_ptr().cast::<TOKEN_OWNER>()).Owner
        })?;
        let identity = OwnIdentity {
            user: sid_string(user.sid())?,
            default_owner,
        };
        Some((identity, user))
    }

    /// Our token's `TOKEN_USER`. Only `own_identity` builds one, from a buffer the
    /// kernel filled for that class.
    pub(super) struct OwnUser(Vec<u8>);

    impl OwnUser {
        /// The SID inside, valid while `self` lives.
        pub(super) fn sid(&self) -> PSID {
            // SAFETY: `own_identity` is the only constructor, and it keeps only a
            // buffer GetTokenInformation(TokenUser) filled. Read unaligned: a
            // `Vec<u8>` promises no more.
            unsafe {
                ptr::read_unaligned(self.0.as_ptr().cast::<TOKEN_USER>())
                    .User
                    .Sid
            }
        }
    }

    /// What the holder of the lock makes of the legacy name.
    #[derive(Debug)]
    pub(super) enum Legacy {
        /// Held: an earlier build launched now finds this instance.
        Held(HANDLE),
        /// An earlier build of ours holds it; this launch hands over to it.
        EarlierBuild,
        /// Held by another or unreadable owner, or not openable. The lock that
        /// matters is ours.
        Unavailable,
    }

    /// `name` is `LEGACY_LOCK_PREFIX` plus our SID.
    pub(super) fn legacy_lock(name: &[u16], own: &OwnIdentity) -> Legacy {
        // SAFETY: NUL-terminated name; an owned handle or null.
        let mutex = unsafe { CreateMutexW(ptr::null(), FALSE, name.as_ptr()) };
        if mutex.is_null() {
            return Legacy::Unavailable;
        }
        // SAFETY: a live handle.
        let wait = unsafe { WaitForSingleObject(mutex, 0) };
        if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
            return Legacy::Held(mutex);
        }
        // An earlier build created it with its token's default owner, which
        // `classify_owner` already accepts.
        let earlier = wait == WAIT_TIMEOUT
            && classify_owner(owner_sid(mutex).as_deref(), own) == ObjectOwner::Ours;
        // SAFETY: our handle, closed once.
        unsafe { CloseHandle(mutex) };
        if earlier {
            Legacy::EarlierBuild
        } else {
            Legacy::Unavailable
        }
    }

    /// The owner the kernel keeps on the object behind `handle`. Needs READ_CONTROL,
    /// which GENERIC_WRITE carries.
    pub(super) fn owner_sid(handle: HANDLE) -> Option<String> {
        let mut owner: PSID = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: a live handle; the descriptor is allocated by the call.
        let rc = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let sid = sid_string(owner);
        // SAFETY: the descriptor `owner` points into, freed once after the conversion.
        unsafe { LocalFree(descriptor.cast()) };
        sid
    }

    fn disabled() -> Option<AppLock> {
        Some(AppLock {
            _mutex: MutexGuard {
                handle: ptr::null_mut(),
                legacy: ptr::null_mut(),
                _namespace: None,
            },
            link: LinkChannel::NotAttempted,
            closer: None,
        })
    }

    /// Every name the guard claims: a test can pass ones that meet no running app.
    pub(super) struct Names {
        pub(super) namespace: String,
        pub(super) pipe: Vec<u16>,
        pub(super) legacy_lock: Vec<u16>,
        pub(super) legacy_focus: Vec<u16>,
    }

    impl Names {
        fn of(own: &OwnIdentity) -> Self {
            Self {
                namespace: NAMESPACE.to_owned(),
                pipe: wide(&format!("{LINK_PIPE_PREFIX}{}", own.user)),
                legacy_lock: wide(&format!("{LEGACY_LOCK_PREFIX}{}", own.user)),
                legacy_focus: wide(&format!("{LEGACY_FOCUS_PREFIX}{}", own.user)),
            }
        }
    }

    pub(super) fn acquire_or_signal(url: Option<&str>) -> Option<AppLock> {
        let lock = acquire_with(url, Names::of)?;
        if let Some(closer) = &lock.closer {
            let _ = CLOSER.set(Arc::clone(closer));
        }
        Some(lock)
    }

    /// `acquire_or_signal` on the given names, free of a running app's.
    ///
    /// A launch that finds the lock taken either hands over and leaves, or, once
    /// the holder's pipe is gone because it is leaving, waits for the lock and
    /// becomes the holder, its own link still to be shown. The pipe and the lock
    /// share one deadline.
    pub(super) fn acquire_with(
        url: Option<&str>,
        names: impl FnOnce(&OwnIdentity) -> Names,
    ) -> Option<AppLock> {
        let Some((own, user)) = own_identity() else {
            crate::vprintln!("[LOCK]   SID lookup failed; single-instance guard disabled");
            return disabled();
        };
        let names = names(&own);
        let namespace = match UserNamespace::open(&names.namespace, user.sid()) {
            Ok(namespace) => namespace,
            Err(error) => {
                crate::vprintln!("[LOCK]   private namespace unavailable: {error}");
                return disabled();
            }
        };
        let attributes = namespace.attributes();
        let lock_name = in_namespace(&names.namespace, LOCK_LEAF);
        let focus_name = in_namespace(&names.namespace, FOCUS_LEAF);

        // SAFETY: lock_name is NUL-terminated UTF-16; returns an owned handle or null.
        let mutex = unsafe { CreateMutexW(&attributes, FALSE, lock_name.as_ptr()) };
        if mutex.is_null() {
            // SAFETY: reads this thread's last error, set by the call above.
            crate::vprintln!("[LOCK]   CreateMutexW failed: {}", unsafe {
                GetLastError()
            });
            return disabled();
        }

        // SAFETY: mutex is a live handle; the wait is thread-safe.
        let wait = unsafe { WaitForSingleObject(mutex, 0) };
        if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
            return first_instance(url, mutex, namespace, &names, &own);
        }

        match hand_off(
            url,
            &names.pipe,
            &own,
            &focus_name,
            Some(mutex),
            handoff_deadline(),
        ) {
            HandedOff::Owner => first_instance(url, mutex, namespace, &names, &own),
            HandedOff::Left => {
                // SAFETY: release our handle, letting the kernel reap the object on owner exit.
                unsafe { CloseHandle(mutex) };
                None
            }
        }
    }

    /// The first instance, `mutex` owned by this thread: it listens for focus
    /// signals and links, and the closer is what turns them away later.
    fn first_instance(
        url: Option<&str>,
        mutex: HANDLE,
        namespace: UserNamespace,
        names: &Names,
        own: &OwnIdentity,
    ) -> Option<AppLock> {
        let attributes = namespace.attributes();
        let focus_name = in_namespace(&names.namespace, FOCUS_LEAF);
        let legacy = match legacy_lock(&names.legacy_lock, own) {
            Legacy::Held(handle) => handle,
            Legacy::EarlierBuild => {
                hand_off(
                    url,
                    &names.pipe,
                    own,
                    &names.legacy_focus,
                    None,
                    handoff_deadline(),
                );
                // SAFETY: our own lock, closed once, so the earlier build stays
                // the only instance.
                unsafe { CloseHandle(mutex) };
                return None;
            }
            Legacy::Unavailable => ptr::null_mut(),
        };
        let closing = Arc::new(AtomicBool::new(false));
        // Manual-reset and unnamed: every focus listener waits on it beside its own
        // event, and it stays signalled once closing has begun.
        // SAFETY: no name, no attributes; an owned handle or null.
        let stop = unsafe { CreateEventW(ptr::null(), TRUE, FALSE, ptr::null()) };
        if stop.is_null() {
            crate::verr!("[LOCK]   no stop event; the focus listeners cannot be closed");
        }
        let mut listeners = Listeners {
            focus: Vec::new(),
            link: None,
        };
        // SAFETY: focus_name is NUL-terminated UTF-16; auto-reset, owned handle.
        let event = unsafe { CreateEventW(&attributes, FALSE, FALSE, focus_name.as_ptr()) };
        if !event.is_null() {
            listeners.focus.extend(spawn_focus_listener(event, stop));
        }
        // The event an earlier build signals once it finds the legacy lock held.
        if !legacy.is_null() {
            // SAFETY: NUL-terminated name; auto-reset, owned handle.
            let event =
                unsafe { CreateEventW(ptr::null(), FALSE, FALSE, names.legacy_focus.as_ptr()) };
            if !event.is_null() {
                listeners.focus.extend(spawn_focus_listener(event, stop));
            }
        }
        // The address exists before this function returns, letting a link
        // that arrives in the next instant find something to write to.
        // Created here rather than inside the listener thread, which the
        // caller outruns. A client arriving first got `ERROR_FILE_NOT_FOUND`,
        // and the only retry was gated on the pipe being busy instead.
        let link = match create_instance(&names.pipe) {
            Some(pipe) => {
                listeners.link =
                    spawn_link_listener(names.pipe.clone(), pipe, Arc::clone(&closing));
                LinkChannel::Listening
            }
            // Deep links are done for this run, and the window still comes
            // forward on a focus signal. The verdict travels rather than
            // being announced here; `report_link_channel` speaks it once the
            // log has been rotated.
            None => LinkChannel::Unavailable,
        };
        let closer = Closer {
            closing,
            stop: SendHandle(stop),
            pipe: matches!(link, LinkChannel::Listening).then(|| names.pipe.clone()),
            listeners: Mutex::new(listeners),
        };
        Some(AppLock {
            _mutex: MutexGuard {
                handle: mutex,
                legacy,
                _namespace: Some(namespace),
            },
            link,
            closer: Some(Arc::new(closer)),
        })
    }

    /// What turns the link pipe and the focus events away: the flag the pipe
    /// listener reads, the event the focus listeners wait on beside their own, the
    /// pipe's name to wake its listener with, and the listeners to wait for.
    pub(super) struct Closer {
        closing: Arc<AtomicBool>,
        stop: SendHandle,
        pipe: Option<Vec<u16>>,
        listeners: Mutex<Listeners>,
    }

    /// The listeners closing waits for, the focus ones before the pipe one.
    struct Listeners {
        focus: Vec<JoinHandle<()>>,
        link: Option<JoinHandle<()>>,
    }

    impl Closer {
        /// Stops serving new clients and delivers what a connected one already
        /// wrote. The pipe's name goes with its last instance. A link sent after
        /// this finds no pipe, and its sender waits on the lock.
        pub(super) fn close(&self) {
            // Held throughout: a second caller waits for the first to be done.
            let mut listeners = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
            // Before the signals, which the listeners act on by reading it.
            self.closing.store(true, Ordering::Release);
            if !self.stop.get().is_null() {
                // SAFETY: the stop event is a live handle, never closed.
                unsafe { SetEvent(self.stop.get()) };
            }
            // The focus events go first, so the pipe never leaves while its holder's
            // focus event is still there: a launch tells a leaving holder from a live
            // one without a pipe by that event. One deadline for the group.
            let deadline = Instant::now() + Duration::from_millis(CLOSE_WAIT_MS);
            for listener in listeners.focus.drain(..) {
                if !join_within(listener, deadline.saturating_duration_since(Instant::now())) {
                    crate::verr!("[LOCK]   a focus listener did not stop; its event stays open");
                }
            }
            // The pipe listener sits in ConnectNamedPipe, which only a client ends.
            // One that connects and writes nothing is read as an empty message.
            if let Some(name) = &self.pipe
                && let Some(client) = open_instance(name)
            {
                // SAFETY: our own client end, closed once and unwritten.
                unsafe { CloseHandle(client) };
            }
            if let Some(listener) = listeners.link.take()
                && !join_within(listener, Duration::from_millis(CLOSE_WAIT_MS))
            {
                crate::verr!("[LOCK]   the link listener did not stop; a link may be left unread");
            }
        }
    }

    /// The closer of the lock `acquire_or_signal` handed out, for the sites that
    /// have no handle on it, the lock being a local of `main`.
    static CLOSER: OnceLock<Arc<Closer>> = OnceLock::new();

    pub(super) fn close_channel() {
        if let Some(closer) = CLOSER.get() {
            closer.close();
        }
    }

    /// The deadline a launch that did not get the lock works to, for the pipe and
    /// for the lock alike.
    fn handoff_deadline() -> Instant {
        Instant::now() + Duration::from_millis(HANDOFF_BUDGET_MS)
    }

    /// Whether a launch that did not get the lock ended up owning it.
    enum HandedOff {
        /// The running instance has what this launch carried, or nothing could be
        /// done; the launch ends.
        Left,
        /// The lock came free while waiting: this launch is the first instance.
        Owner,
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
    ///
    /// With the `lock` to wait on, a pipe that is not there is waited out on that
    /// lock until `deadline`, and a launch that gets it ends as the first instance.
    fn hand_off(
        url: Option<&str>,
        pipe_name: &[u16],
        own: &OwnIdentity,
        focus_name: &[u16],
        lock: Option<HANDLE>,
        deadline: Instant,
    ) -> HandedOff {
        let wait = lock.map(|mutex| LockWait { mutex, focus_name });
        let pipe = match connect_link_pipe(pipe_name, own, wait, deadline) {
            Reach::Owner => return HandedOff::Owner,
            Reach::Pipe(pipe) => {
                grant_foreground(pipe);
                Ok(pipe)
            }
            Reach::Lost(unanswered) => {
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
                return HandedOff::Left;
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
        // SAFETY: focus_name is NUL-terminated UTF-16; null if no event exists.
        let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, FALSE, focus_name.as_ptr()) };
        if event.is_null() {
            // Ungated for the reason above: a focus request that reaches nobody.
            crate::verr!(
                "[LOCK]   the running instance's focus event is gone; nothing was raised (gle={})",
                // SAFETY: reads this thread's last error, set by the call above.
                unsafe { GetLastError() }
            );
            return HandedOff::Left;
        }
        // SAFETY: event opened with EVENT_MODIFY_STATE; signalled then closed.
        unsafe {
            if SetEvent(event) == 0 {
                crate::verr!(
                    "[LOCK]   the focus signal was not set (gle={})",
                    GetLastError()
                );
            }
            CloseHandle(event);
        }
        HandedOff::Left
    }

    /// One connection at a time, serially. A deep link arrives on a click, not in
    /// a stream, and a queue here would only let a stalled reader hide the next one.
    /// Past `closing` it serves the connection in hand and makes no next instance,
    /// the name going with the last.
    fn spawn_link_listener(
        pipe_name: Vec<u16>,
        first: HANDLE,
        closing: Arc<AtomicBool>,
    ) -> Option<JoinHandle<()>> {
        let first = SendHandle(first);
        std::thread::Builder::new()
            .name("app-link-listener".to_owned())
            .spawn(move || {
                let mut pipe = first.get();
                loop {
                    // SAFETY: freshly created server end; blocks until a client connects.
                    unsafe { ConnectNamedPipe(pipe, ptr::null_mut()) };

                    // The next instance before this conversation ends, not after,
                    // since the name is the address clients look for and closing
                    // the only instance would take the address down with it.
                    let next = if closing.load(Ordering::Acquire) {
                        None
                    } else {
                        create_instance(&pipe_name)
                    };
                    serve_instance(pipe);
                    match next {
                        Some(handle) => pipe = handle,
                        None => return,
                    }
                }
            })
            .ok()
    }

    /// One server end of the link pipe, or `None` once the name is unusable.
    pub(super) fn create_instance(pipe_name: &[u16]) -> Option<HANDLE> {
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
            // SAFETY: reads this thread's last error, set by the call above.
            crate::vprintln!("[LOCK]   Link pipe unavailable: {}", unsafe {
                GetLastError()
            });
            return None;
        }
        Some(pipe)
    }

    /// The client end of the link pipe, or `None` when no free instance answers.
    ///
    /// Anonymous: whoever serves the name gets no token of ours to impersonate, even
    /// before `connect_link_pipe` has checked who that is.
    pub(super) fn open_instance(pipe_name: &[u16]) -> Option<HANDLE> {
        // SAFETY: pipe_name is NUL-terminated UTF-16; returns an owned handle or
        // INVALID_HANDLE_VALUE when no free server end exists.
        let pipe = unsafe {
            CreateFileW(
                pipe_name.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_NONE,
                ptr::null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS,
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

    /// How a launch that found the lock taken got to the instance holding it.
    pub(super) enum Reach {
        /// A client end of the link pipe, its server known to be ours.
        Pipe(HANDLE),
        /// The lock came free while the pipe was away: this launch owns it now.
        Owner,
        /// Neither, and why.
        Lost(HandOff),
    }

    /// Whether `lock` was just taken by this thread: free, or left by an owner whose
    /// thread ended, which is how a holder that exits releases it.
    fn took(lock: HANDLE, wait_ms: u32) -> bool {
        // SAFETY: a live mutex handle; the wait is thread-safe.
        let wait = unsafe { WaitForSingleObject(lock, wait_ms) };
        wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED
    }

    /// The lock a launch waits on, and the focus event of the holder it waits for.
    #[derive(Clone, Copy)]
    pub(super) struct LockWait<'a> {
        pub(super) mutex: HANDLE,
        pub(super) focus_name: &'a [u16],
    }

    /// Whether the holder still serves focus requests. Its listener closes the only
    /// handle to the event when it stops, and an event is destroyed with its last
    /// handle (CreateEventW, CloseHandle docs); the name is gone with it. This
    /// launch's own handle lives for the length of the call.
    fn focus_event_exists(focus_name: &[u16]) -> bool {
        // SAFETY: focus_name is NUL-terminated UTF-16; null if no event exists.
        let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, FALSE, focus_name.as_ptr()) };
        if event.is_null() {
            // SAFETY: reads this thread's last error, set by the call above.
            return unsafe { GetLastError() } != ERROR_FILE_NOT_FOUND;
        }
        // SAFETY: the handle opened above, closed once.
        unsafe { CloseHandle(event) };
        true
    }

    /// Retries only while the OS says the answer could still change.
    ///
    /// The retry is bounded by diagnosis rather than by patience: a busy pipe and
    /// an absent one are both transient and get the budget, anything else is
    /// answered once and reported. The previous single attempt lost the startup
    /// race outright.
    ///
    /// A pipe that is absent is not made yet, or no longer there, or never was. With
    /// a `lock` to wait on, the pause between looks is spent on it, freeing this
    /// launch the moment the holder is gone. A holder still serving focus gets only
    /// the startup grace; one that stopped serving it is closing, and gets the budget.
    ///
    /// The one site a client handle is born, and the handle leaves it only once the
    /// kernel names its server as ours: nothing is granted to, or written into, a name
    /// another account claimed first. A foreign holder is not retried, since our own
    /// server cannot coexist with it.
    pub(super) fn connect_link_pipe(
        pipe_name: &[u16],
        own: &OwnIdentity,
        lock: Option<LockWait<'_>>,
        deadline: Instant,
    ) -> Reach {
        let mut alive_since: Option<Instant> = None;
        loop {
            if let Some(pipe) = open_instance(pipe_name) {
                let refused = match classify_owner(owner_sid(pipe).as_deref(), own) {
                    ObjectOwner::Ours => return Reach::Pipe(pipe),
                    ObjectOwner::Foreign(sid) => {
                        crate::vprintln!("[LOCK]   link pipe held by {sid}, refused");
                        HandOff::ForeignServer
                    }
                    ObjectOwner::Unattributed => HandOff::UnattributedServer,
                };
                // SAFETY: our own client end, closed once and unwritten.
                unsafe { CloseHandle(pipe) };
                return Reach::Lost(refused);
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
                    if lock.is_some_and(|lock| took(lock.mutex, 0)) {
                        return Reach::Owner;
                    }
                    HandOff::Busy
                }
                // The name is not there. `WaitNamedPipeW` needs one; the pause goes
                // to the lock, or to a clock when there is none.
                ERROR_FILE_NOT_FOUND => {
                    match lock {
                        Some(lock) if took(lock.mutex, LINK_RETRY_PAUSE_MS) => {
                            return Reach::Owner;
                        }
                        // Alive (still serving focus) and without a pipe, or not
                        // yet with one: it gets the grace the startup race needs,
                        // not the budget. Looked at again each time, since a holder
                        // that is closing drops its focus event before its pipe.
                        Some(lock) if focus_event_exists(lock.focus_name) => {
                            let since = *alive_since.get_or_insert_with(Instant::now);
                            if since.elapsed() >= Duration::from_millis(NO_LISTENER_GRACE_MS) {
                                return Reach::Lost(HandOff::NoListener);
                            }
                        }
                        Some(_) => {}
                        None => std::thread::sleep(Duration::from_millis(u64::from(
                            LINK_RETRY_PAUSE_MS,
                        ))),
                    }
                    HandOff::NoListener
                }
                // Not a race, since another attempt cannot answer differently.
                _ => return Reach::Lost(HandOff::Failed),
            };
            if Instant::now() >= deadline {
                return Reach::Lost(unanswered);
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

    /// Serves `event` until `stop` is signalled, then closes it: a wait on a handle
    /// closed under it is undefined; the thread that waits is the one that closes,
    /// and the name goes with it. A null `stop` is never signalled.
    fn spawn_focus_listener(event: HANDLE, stop: HANDLE) -> Option<JoinHandle<()>> {
        let event = SendHandle(event);
        let stop = SendHandle(stop);
        std::thread::Builder::new()
            .name("app-focus-listener".to_owned())
            .spawn(move || {
                let handles = [event.get(), stop.get()];
                let count = if stop.get().is_null() { 1 } else { 2 };
                loop {
                    // SAFETY: two live handles, or one when `stop` is null; neither is
                    // closed while this wait runs, and the wait is thread-safe.
                    let r =
                        unsafe { WaitForMultipleObjects(count, handles.as_ptr(), FALSE, INFINITE) };
                    // The event is the first handle; the stop event, or a failure, ends it.
                    if r != WAIT_OBJECT_0 {
                        break;
                    }
                    post_focus();
                }
                // SAFETY: our event, waited on nowhere else and closed once.
                unsafe { CloseHandle(event.get()) };
            })
            .ok()
    }
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::fs::{File, OpenOptions, TryLockError};
    use std::hash::Hasher;
    use std::io::ErrorKind;
    use std::net::Shutdown;
    use std::os::fd::AsRawFd;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use super::{
        AppLock, CLOSE_WAIT_MS, HANDOFF_BUDGET_MS, HandOff, NO_LISTENER_GRACE_MS, join_within,
        post_focus,
    };

    /// Who sent a datagram, as the kernel reports it rather than as the payload
    /// claims.
    ///
    /// The channel's own socket sits in a directory no other account can enter,
    /// but the legacy abstract name is still bound, and `unix(7)` is explicit that
    /// an abstract socket has no permission model: every process in the same
    /// network namespace can reach it, whoever owns it.
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

    /// How often a launch waiting for the lock tries it again: `std` has no lock
    /// wait with a deadline, and a try is one syscall.
    const LOCK_POLL_MS: u64 = 10;

    /// The lock is a file in the data directory it guards, so every launch that
    /// would share the database takes the same lock, whatever its environment.
    const LOCK_FILE: &str = "instance.lock";

    /// Group and other bits a channel directory must not carry: its socket is
    /// reachable by whoever can enter it.
    pub(super) const CHANNEL_DIR_FORBIDDEN: u32 = 0o077;

    /// The channel's directory inside the data directory, when the runtime one
    /// is not private.
    pub(super) const FALLBACK_DIR: &str = "run";

    /// Owner read-write, plus the sticky bit the XDG spec names for a file of the
    /// runtime directory that must survive its periodic clean-up.
    pub(super) const CHANNEL_MODE: u32 = 0o1600;

    fn data_dir_hash(data_dir: &Path) -> u64 {
        let mut hasher = fnv::FnvHasher::default();
        hasher.write(data_dir.to_string_lossy().as_bytes());
        hasher.finish()
    }

    /// Keyed on the data directory, like the lock: two data directories of one
    /// account never share a channel.
    pub(super) fn channel_name(data_dir: &Path) -> String {
        format!("tidalunar-{:016x}.sock", data_dir_hash(data_dir))
    }

    /// The name earlier builds bind. No longer the lock (any account can hold an
    /// abstract name), it stays bound for an earlier build launched now to find.
    pub(super) fn legacy_addr(data_dir: &Path) -> std::io::Result<SocketAddr> {
        SocketAddr::from_abstract_name(format!("tidalunar-app-{:016x}", data_dir_hash(data_dir)))
    }

    /// Why a directory cannot carry the lock or the channel.
    #[derive(Debug, PartialEq)]
    pub(super) enum DirRefusal {
        Missing,
        /// A file, or a symlink: what gets checked must be what gets used.
        NotADirectory,
        /// Owned by another uid, kept for the log line.
        NotOurs(u32),
        /// The mode, carrying a bit `forbidden` names.
        OpenToOthers(u32),
    }

    /// A real directory, owned by `own`, with none of the `forbidden` bits.
    ///
    /// Checked on the path, not a descriptor: replacing the directory between
    /// this check and the bind needs write access to its parent, which is ours
    /// for `FALLBACK_DIR` and root's for `/run/user`.
    pub(super) fn check_dir(path: &Path, own: u32, forbidden: u32) -> Result<(), DirRefusal> {
        check_meta(std::fs::symlink_metadata(path), own, forbidden)
    }

    /// The data directory, owned by `own`, its mode unchecked: the database lives
    /// there too, and a lock is no more private than it. It may be a symlink, as
    /// when its owner moved it to another disk; what must be ours is where it
    /// leads, and the link sits in a parent only its owner can change.
    pub(super) fn check_data_dir(path: &Path, own: u32) -> Result<(), DirRefusal> {
        check_meta(std::fs::metadata(path), own, 0)
    }

    fn check_meta(
        meta: std::io::Result<std::fs::Metadata>,
        own: u32,
        forbidden: u32,
    ) -> Result<(), DirRefusal> {
        let meta = meta.map_err(|_| DirRefusal::Missing)?;
        if !meta.file_type().is_dir() {
            return Err(DirRefusal::NotADirectory);
        }
        if meta.uid() != own {
            return Err(DirRefusal::NotOurs(meta.uid()));
        }
        let mode = meta.mode() & 0o7777;
        if mode & forbidden != 0 {
            return Err(DirRefusal::OpenToOthers(mode));
        }
        Ok(())
    }

    /// Where the channel's socket lives: the session's runtime directory when it
    /// is as private as the XDG spec promises, else our own `FALLBACK_DIR` in the
    /// data directory. `None` when neither is private.
    pub(super) fn channel_dir(
        data_dir: &Path,
        runtime: Option<PathBuf>,
        own: u32,
    ) -> Option<PathBuf> {
        if let Some(dir) = runtime.filter(|dir| dir.is_absolute())
            && check_dir(&dir, own, CHANNEL_DIR_FORBIDDEN).is_ok()
        {
            return Some(dir);
        }
        let run = data_dir.join(FALLBACK_DIR);
        match std::fs::DirBuilder::new().mode(0o700).create(&run) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(_) => return None,
        }
        check_dir(&run, own, CHANNEL_DIR_FORBIDDEN)
            .is_ok()
            .then_some(run)
    }

    /// The bound end of the channel. Its file is never unlinked on exit: only
    /// `bind` removes one, under the lock. A successor's socket is out of reach
    /// of the leaving holder.
    pub(super) struct Channel {
        pub(super) socket: UnixDatagram,
        pub(super) path: PathBuf,
    }

    impl Channel {
        /// Only the lock holder binds, so a socket file already there belongs to a
        /// holder that is gone, and unlinking it cannot take a live one. A channel
        /// exists only with `CHANNEL_MODE` set.
        fn bind(path: PathBuf) -> std::io::Result<Self> {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let channel = Self {
                socket: UnixDatagram::bind(&path)?,
                path,
            };
            // Still under the lock: a refused mode leaves no socket behind.
            if let Err(e) = std::fs::set_permissions(
                &channel.path,
                std::fs::Permissions::from_mode(CHANNEL_MODE),
            ) {
                let _ = std::fs::remove_file(&channel.path);
                return Err(e);
            }
            Ok(channel)
        }
    }

    /// What a launch finds for its data directory.
    pub(super) enum Claim {
        /// This process holds the lock. `channel` is the reason when none could
        /// be bound: the lock still holds, but neither focus nor links can arrive.
        Holder {
            lock: File,
            channel: Result<Channel, String>,
        },
        /// Another process of this account holds it, listening here; `None` when
        /// no private directory could name its channel.
        Held(Option<SocketAddr>),
        /// The guard cannot stand, and says why.
        Unguarded(String),
    }

    /// Takes the data directory's lock, or names the channel of whoever holds it.
    pub(super) fn claim(data_dir: &Path, runtime: Option<PathBuf>, own: u32) -> Claim {
        if std::fs::create_dir_all(data_dir).is_err() {
            return Claim::Unguarded("the data directory cannot be created".into());
        }
        if let Err(refusal) = check_data_dir(data_dir, own) {
            return Claim::Unguarded(format!("the data directory is refused ({refusal:?})"));
        }
        let Ok(lock) = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(data_dir.join(LOCK_FILE))
        else {
            return Claim::Unguarded("the lock file cannot be opened".into());
        };
        let path = channel_dir(data_dir, runtime, own).map(|dir| dir.join(channel_name(data_dir)));
        match lock.try_lock() {
            Ok(()) => Claim::Holder {
                lock,
                channel: match path {
                    Some(path) => Channel::bind(path)
                        .map_err(|e| format!("the channel cannot be bound ({e})")),
                    None => Err("no private directory for the channel".into()),
                },
            },
            Err(TryLockError::WouldBlock) => {
                Claim::Held(path.and_then(|path| SocketAddr::from_pathname(path).ok()))
            }
            Err(TryLockError::Error(_)) => {
                Claim::Unguarded("the lock file cannot be locked".into())
            }
        }
    }

    /// `(hostname, pid)` from Chromium's `SingletonLock` symlink target,
    /// `<hostname>-<pid>`. The hostname may itself hold dashes.
    pub(super) fn parse_singleton_lock(target: &str) -> Option<(&str, u32)> {
        let (host, pid) = target.rsplit_once('-')?;
        if host.is_empty() {
            return None;
        }
        Some((host, pid.parse().ok()?))
    }

    pub(super) fn hostname() -> Option<Vec<u8>> {
        let mut name = [0u8; 256];
        // SAFETY: the buffer is ours and its length is passed; gethostname writes
        // at most that many bytes.
        if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } != 0 {
            return None;
        }
        let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        Some(name[..len].to_vec())
    }

    /// An earlier build of ours runs on this data directory: CEF's `SingletonLock`
    /// names a live process of this host and account, with our executable name.
    /// Such a build takes no file lock; the symlink is its one attributable trace,
    /// present once CEF is up. The path is its layout, whatever later builds use.
    fn earlier_build_running(data_dir: &Path, own: u32) -> bool {
        let Ok(target) = std::fs::read_link(data_dir.join("cef").join("SingletonLock")) else {
            return false;
        };
        let Some((host, pid)) = target.to_str().and_then(parse_singleton_lock) else {
            return false;
        };
        if hostname().as_deref() != Some(host.as_bytes()) {
            return false;
        }
        let proc = PathBuf::from(format!("/proc/{pid}"));
        let ours = std::fs::metadata(&proc).is_ok_and(|meta| meta.uid() == own);
        let exe = std::fs::read_link(proc.join("exe")).ok();
        let own_exe = std::env::current_exe().ok();
        ours && exe.is_some()
            && exe.as_deref().and_then(Path::file_name)
                == own_exe.as_deref().and_then(Path::file_name)
    }

    /// Asks for the credentials of every datagram, then listens. A failed ask is
    /// logged, not fatal: every datagram then arrives unattributed and is refused.
    /// Gives back what closing needs: a handle on the socket and the listener.
    fn listen(
        socket: &UnixDatagram,
        closing: &Arc<AtomicBool>,
    ) -> Option<(UnixDatagram, JoinHandle<()>)> {
        if let Err(e) = enable_peer_credentials(socket) {
            crate::vprintln!("[LOCK]   no peer credentials ({e}); signals will be refused");
        }
        let for_listener = socket.try_clone().ok()?;
        let for_closing = socket.try_clone().ok()?;
        let listener = spawn_focus_listener(for_listener, Arc::clone(closing))?;
        Some((for_closing, listener))
    }

    /// What turns the channel away: the flag the listeners read, a handle on each
    /// socket to shut, and the listeners to wait for.
    pub(super) struct Closer {
        closing: Arc<AtomicBool>,
        sockets: Vec<UnixDatagram>,
        listeners: Mutex<Vec<JoinHandle<()>>>,
    }

    impl Closer {
        /// Stops reading new datagrams and delivers what already arrived. A sender
        /// after this gets `EPIPE`, the file staying where it is (only `bind`
        /// removes it), and the pending link is final once this returns.
        pub(super) fn close(&self) {
            // Held throughout: a second caller waits for the first to be done.
            let mut listeners = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
            // Before the shutdown, which the listeners can only tell from an empty
            // datagram by this flag.
            self.closing.store(true, Ordering::Release);
            for socket in &self.sockets {
                if let Err(e) = socket.shutdown(Shutdown::Read) {
                    crate::verr!(
                        "[LOCK]   the channel could not be closed ({e}); links may still arrive"
                    );
                }
            }
            for listener in listeners.drain(..) {
                if !join_within(listener, Duration::from_millis(CLOSE_WAIT_MS)) {
                    crate::verr!(
                        "[LOCK]   a channel listener did not stop; a link may be left unread"
                    );
                }
            }
        }
    }

    /// The closer of the guard `acquire_or_signal` handed out, for the sites that
    /// have no handle on the lock, which is a local of `main`.
    static CLOSER: OnceLock<Arc<Closer>> = OnceLock::new();

    pub(super) fn close_channel() {
        if let Some(closer) = CLOSER.get() {
            closer.close();
        }
    }

    /// What the holder of the lock makes of the legacy name.
    pub(super) enum Legacy {
        /// Bound: an earlier build launched now hands over to this instance.
        Bound(UnixDatagram),
        /// An earlier build of ours holds it and runs on this data directory;
        /// this launch hands over to it instead of starting beside it.
        EarlierBuild(SocketAddr),
        /// Held by a process nobody can attribute, possibly another account's,
        /// or no name at all. This instance holds the lock that matters.
        Unavailable,
    }

    pub(super) fn legacy(data_dir: &Path, own: u32) -> Legacy {
        let Ok(addr) = legacy_addr(data_dir) else {
            return Legacy::Unavailable;
        };
        match UnixDatagram::bind_addr(&addr) {
            Ok(socket) => Legacy::Bound(socket),
            Err(e) if e.kind() == ErrorKind::AddrInUse && earlier_build_running(data_dir, own) => {
                Legacy::EarlierBuild(addr)
            }
            Err(_) => Legacy::Unavailable,
        }
    }

    /// Held for the process lifetime.
    pub(crate) struct Guard {
        _lock: File,
        _channel: Option<Channel>,
        _legacy: Option<UnixDatagram>,
        pub(super) closer: Arc<Closer>,
    }

    pub(super) fn acquire_or_signal(url: Option<&str>) -> Option<AppLock> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        let lock = acquire_in(&crate::state::cache_data_dir(), runtime, url)?;
        if let Some(guard) = &lock._guard {
            let _ = CLOSER.set(Arc::clone(&guard.closer));
        }
        Some(lock)
    }

    /// `acquire_or_signal` on the given directories (a test needs no global). Same
    /// hand-over and takeover as the Windows twin, under one shared deadline.
    pub(super) fn acquire_in(
        data_dir: &Path,
        runtime: Option<PathBuf>,
        url: Option<&str>,
    ) -> Option<AppLock> {
        let own = own_uid();
        let deadline = Instant::now() + Duration::from_millis(HANDOFF_BUDGET_MS);
        let mut unbound_since: Option<Instant> = None;
        let mut waited = false;
        loop {
            match claim(data_dir, runtime.clone(), own) {
                Claim::Holder { lock, channel } => {
                    if waited {
                        crate::vprintln!("[LOCK]   The previous instance has left; taking over");
                    }
                    return hold(data_dir, own, lock, channel, url);
                }
                Claim::Held(addr) => {
                    let outcome = match &addr {
                        Some(addr) => hand_over(addr, url),
                        None => HandOff::NoListener,
                    };
                    // A leaving holder is waited for to the deadline. No socket is
                    // not told apart from a holder that has not bound yet: it only
                    // gets the grace that race needs.
                    let patience = match (&outcome, &addr) {
                        (HandOff::Closing, _) => Some(deadline),
                        (HandOff::NoListener, Some(_)) => {
                            let since = *unbound_since.get_or_insert_with(Instant::now);
                            let grace = Duration::from_millis(NO_LISTENER_GRACE_MS);
                            Some((since + grace).min(deadline))
                        }
                        _ => None,
                    };
                    if patience.is_some_and(|until| Instant::now() < until) {
                        waited = true;
                        std::thread::sleep(Duration::from_millis(LOCK_POLL_MS));
                        continue;
                    }
                    report_hand_off(url, &outcome);
                    return None;
                }
                Claim::Unguarded(reason) => {
                    crate::vprintln!("[LOCK]   {reason}; guard disabled");
                    return Some(AppLock { _guard: None });
                }
            }
        }
    }

    /// The first instance: its channel and the legacy name listen, and `closer`
    /// is what turns them away later.
    fn hold(
        data_dir: &Path,
        own: u32,
        lock: File,
        channel: Result<Channel, String>,
        url: Option<&str>,
    ) -> Option<AppLock> {
        let legacy_socket = match legacy(data_dir, own) {
            Legacy::Bound(socket) => Some(socket),
            Legacy::EarlierBuild(addr) => {
                report_hand_off(url, &hand_over(&addr, url));
                return None;
            }
            Legacy::Unavailable => None,
        };
        let channel = channel
            .inspect_err(|reason| {
                crate::vprintln!("[LOCK]   {reason}; focus and links cannot arrive");
            })
            .ok();
        let closing = Arc::new(AtomicBool::new(false));
        let (sockets, listeners) = channel
            .iter()
            .map(|c| &c.socket)
            .chain(legacy_socket.iter())
            .filter_map(|socket| listen(socket, &closing))
            .unzip();
        Some(AppLock {
            _guard: Some(Guard {
                _lock: lock,
                _channel: channel,
                _legacy: legacy_socket,
                closer: Arc::new(Closer {
                    closing,
                    sockets,
                    listeners: Mutex::new(listeners),
                }),
            }),
        })
    }

    fn report_hand_off(url: Option<&str>, outcome: &HandOff) {
        match (url, outcome) {
            (Some(_), HandOff::Landed) => {
                crate::vprintln!("[LOCK]   Handed a deep link to the running instance");
            }
            // Ungated like the rest: the wait ended with the old instance still
            // there, and the link it was waiting to carry goes nowhere.
            (Some(_), HandOff::Closing) => crate::verr!(
                "[LOCK]   deep link lost: the running instance was still closing after the wait"
            ),
            (None, HandOff::Closing) => crate::verr!(
                "[LOCK]   not started: the running instance was still closing after the wait"
            ),
            // Ungated: this process exits before reading the persisted log level,
            // and a click whose link never opens leaves no other trace.
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
        match client.send(url.as_bytes()) {
            Ok(_) => return HandOff::Landed,
            // No focus word either: it would be turned away the same way.
            Err(e) if closed_by_peer(&e) => return HandOff::Closing,
            Err(_) => {}
        }
        // The link did not go; the window still should.
        let _ = client.send(FOCUS_SIGNAL.as_bytes());
        HandOff::Failed
    }

    fn send_or_fail(client: &UnixDatagram, word: &str) -> HandOff {
        match client.send(word.as_bytes()) {
            Ok(_) => HandOff::Landed,
            Err(e) if closed_by_peer(&e) => HandOff::Closing,
            Err(_) => HandOff::Failed,
        }
    }

    /// What a send gets once the receiver shut its reading end (`EPIPE`) or closed
    /// its socket after we connected (`ECONNREFUSED`). Only a send says so: a
    /// connect to a socket nobody serves is a holder not bound yet, or never bound.
    fn closed_by_peer(error: &std::io::Error) -> bool {
        matches!(
            error.kind(),
            ErrorKind::BrokenPipe | ErrorKind::ConnectionRefused
        )
    }

    fn spawn_focus_listener(
        socket: UnixDatagram,
        closing: Arc<AtomicBool>,
    ) -> Option<JoinHandle<()>> {
        std::thread::Builder::new()
            .name("app-focus-listener".to_owned())
            .spawn(move || {
                let own = own_uid();
                let mut buf = [0u8; MAX_SIGNAL];
                // The length matters now that the payload varies (a receive that
                // ignored it would read stale bytes from the previous datagram).
                while let Ok((n, peer)) = recv_with_sender(&socket, &mut buf) {
                    // A shut socket reads as empty once its queue is drained, as an
                    // empty datagram does; only the flag tells them apart.
                    if n == 0 && closing.load(Ordering::Acquire) {
                        break;
                    }
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
            })
            .ok()
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

#[cfg(test)]
#[path = "../../tests/unit/platform/app_lock_server.rs"]
mod server_tests;
