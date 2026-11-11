use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_SUCCESS, FALSE,
    GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree, TRUE, WAIT_ABANDONED,
    WAIT_OBJECT_0, WAIT_TIMEOUT, WIN32_ERROR,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, TOKEN_INFORMATION_CLASS, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER, TokenOwner,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_INBOUND, ReadFile, SECURITY_ANONYMOUS,
    SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId,
    PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{
    AddSIDToBoundaryDescriptor, ClosePrivateNamespace, CreateBoundaryDescriptorW, CreateEventW,
    CreateMutexW, CreatePrivateNamespaceW, DeleteBoundaryDescriptor, EVENT_MODIFY_STATE,
    GetCurrentProcess, INFINITE, OpenEventW, OpenPrivateNamespaceW, OpenProcessToken, SetEvent,
    WaitForMultipleObjects, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

use super::{
    AppLock, Attempt, CLOSE_WAIT_MS, Focus, HANDOFF_BUDGET_MS, HandOff, LinkChannel,
    NO_LISTENER_GRACE_MS, ObjectOwner, OwnIdentity, Unguarded, Unserved, classify_owner,
    join_within, post_focus,
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
// by WaitForMultipleObjects/SetEvent/CloseHandle, the link pipe by ConnectNamedPipe,
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
    let ok =
        unsafe { GetTokenInformation(token, class, buf.as_mut_ptr().cast(), needed, &mut needed) };
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
    let default_owner =
        sid_string(unsafe { ptr::read_unaligned(owner?.as_ptr().cast::<TOKEN_OWNER>()).Owner })?;
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

fn disabled(cause: Unguarded) -> Option<AppLock> {
    Some(AppLock {
        _mutex: MutexGuard {
            handle: ptr::null_mut(),
            legacy: ptr::null_mut(),
            _namespace: None,
        },
        link: LinkChannel::NotAttempted,
        closer: None,
        unserved: vec![Unserved::Unguarded(cause)],
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
        return disabled(Unguarded::Identity);
    };
    let names = names(&own);
    let namespace = match UserNamespace::open(&names.namespace, user.sid()) {
        Ok(namespace) => namespace,
        Err(error) => return disabled(Unguarded::Namespace(error)),
    };
    let attributes = namespace.attributes();
    let lock_name = in_namespace(&names.namespace, LOCK_LEAF);
    let focus_name = in_namespace(&names.namespace, FOCUS_LEAF);

    // SAFETY: lock_name is NUL-terminated UTF-16; returns an owned handle or null.
    let mutex = unsafe { CreateMutexW(&attributes, FALSE, lock_name.as_ptr()) };
    if mutex.is_null() {
        // SAFETY: reads this thread's last error, set by the call above.
        return disabled(Unguarded::Mutex(unsafe { GetLastError() }));
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
    let mut unserved = Vec::new();
    if stop.is_null() {
        unserved.push(Unserved::Stop(std::io::Error::last_os_error()));
    }
    let mut listeners = Listeners {
        focus: Vec::new(),
        link: None,
        kept: Vec::new(),
    };
    // SAFETY: focus_name is NUL-terminated UTF-16; auto-reset, owned handle.
    let event = unsafe { CreateEventW(&attributes, FALSE, FALSE, focus_name.as_ptr()) };
    if event.is_null() {
        unserved.push(Unserved::Focus(std::io::Error::last_os_error()));
    } else {
        match spawn_focus_listener(event, stop) {
            Ok(listener) => listeners.focus.push(listener),
            Err(e) => {
                // Kept until closing begins: its presence is what tells a launch
                // this holder is not leaving, whether or not a thread reads it.
                listeners.kept.push(SendHandle(event));
                unserved.push(Unserved::Focus(e));
            }
        }
    }
    // The event an earlier build signals once it finds the legacy lock held.
    if !legacy.is_null() {
        // SAFETY: NUL-terminated name; auto-reset, owned handle.
        let event = unsafe { CreateEventW(ptr::null(), FALSE, FALSE, names.legacy_focus.as_ptr()) };
        if event.is_null() {
            unserved.push(Unserved::LegacyFocus(std::io::Error::last_os_error()));
        } else {
            match spawn_focus_listener(event, stop) {
                Ok(listener) => listeners.focus.push(listener),
                Err(e) => {
                    // SAFETY: no thread took it; it is ours alone, closed once. An
                    // earlier build reads nothing from it but the signal.
                    unsafe { CloseHandle(event) };
                    unserved.push(Unserved::LegacyFocus(e));
                }
            }
        }
    }
    // The address exists before this function returns, letting a link
    // that arrives in the next instant find something to write to.
    // Created here rather than inside the listener thread, which the
    // caller outruns. A client arriving first got `ERROR_FILE_NOT_FOUND`,
    // and the only retry was gated on the pipe being busy instead.
    let link = match create_instance(&names.pipe) {
        Some(pipe) => match spawn_link_listener(names.pipe.clone(), pipe, Arc::clone(&closing)) {
            Ok(listener) => {
                listeners.link = Some(listener);
                LinkChannel::Listening
            }
            Err(e) => {
                // SAFETY: no thread took it; it is ours alone, closed once. A launch then
                // finds no pipe instead of one that takes its link and never reads.
                unsafe { CloseHandle(pipe) };
                unserved.push(Unserved::Link(e));
                LinkChannel::Unavailable
            }
        },
        // Deep links are done for this run, and the window still comes
        // forward on a focus signal. The verdict travels rather than
        // being announced here; `report_channels` speaks it once the
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
        unserved,
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
    /// Focus events no thread serves, closed with the others.
    kept: Vec<SendHandle>,
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
        for event in listeners.kept.drain(..) {
            // SAFETY: no thread waits on it and nothing else closes it; closed here, once.
            unsafe { CloseHandle(event.get()) };
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
    if let Some(HandOff::Landed) = handoff {
        crate::vprintln!("[LOCK]   Handed a deep link to the running instance");
        return HandedOff::Left;
    }
    // Asked before the verdict is logged, letting the line say what happened to the window.
    let focus = ask_focus(focus_name);
    match handoff {
        Some(link) => Attempt { link, focus }.report_lost(),
        None if focus == Focus::Asked => {
            crate::vprintln!("[LOCK]   Another instance is running; {focus}");
        }
        // Ungated: a focus request that reaches nobody.
        None => crate::verr!("[LOCK]   Another instance is running; {focus}"),
    }
    HandedOff::Left
}

/// Signals the running instance's focus event, and says how far that got.
fn ask_focus(focus_name: &[u16]) -> Focus {
    // SAFETY: focus_name is NUL-terminated UTF-16; null if no event exists.
    let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, FALSE, focus_name.as_ptr()) };
    if event.is_null() {
        // SAFETY: reads this thread's last error, set by the call above.
        return Focus::Gone(unsafe { GetLastError() });
    }
    // SAFETY: event opened with EVENT_MODIFY_STATE; signalled then closed, with
    // the error read before the close.
    unsafe {
        let focus = if SetEvent(event) == 0 {
            Focus::Unset(GetLastError())
        } else {
            Focus::Asked
        };
        CloseHandle(event);
        focus
    }
}

/// One connection at a time, serially. A deep link arrives on a click, not in
/// a stream, and a queue here would only let a stalled reader hide the next one.
/// Past `closing` it serves the connection in hand and makes no next instance,
/// the name going with the last. A failed start leaves the pipe with the caller.
fn spawn_link_listener(
    pipe_name: Vec<u16>,
    first: HANDLE,
    closing: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    let served = SendHandle(first);
    std::thread::Builder::new()
        .name("app-link-listener".to_owned())
        .spawn(move || {
            let mut pipe = served.get();
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
                    None => {
                        std::thread::sleep(Duration::from_millis(u64::from(LINK_RETRY_PAUSE_MS)))
                    }
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
/// the whole session). A refusal is ungated for the reason the lost-link report
/// is (see `hand_off`); a success speaks only under `LOGS`, as the others there do.
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
        crate::vprintln!("[LOCK]   foreground granted to pid {pid}");
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
/// and the name goes with it. A null `stop` is never signalled. A failed start
/// leaves the event with the caller.
fn spawn_focus_listener(event: HANDLE, stop: HANDLE) -> std::io::Result<JoinHandle<()>> {
    let served = SendHandle(event);
    let stop = SendHandle(stop);
    std::thread::Builder::new()
        .name("app-focus-listener".to_owned())
        .spawn(move || {
            let handles = [served.get(), stop.get()];
            let count = if stop.get().is_null() { 1 } else { 2 };
            loop {
                // SAFETY: two live handles, or one when `stop` is null; neither is
                // closed while this wait runs, and the wait is thread-safe.
                let r = unsafe { WaitForMultipleObjects(count, handles.as_ptr(), FALSE, INFINITE) };
                // The event is the first handle; the stop event, or a failure, ends it.
                if r != WAIT_OBJECT_0 {
                    break;
                }
                post_focus();
            }
            // SAFETY: our event, waited on nowhere else and closed once.
            unsafe { CloseHandle(served.get()) };
        })
}

#[cfg(test)]
#[path = "../../../tests/unit/platform/app_lock/windows_impl.rs"]
mod tests;
