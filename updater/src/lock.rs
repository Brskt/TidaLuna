//! Win32 install/update mutex, `TidaLunarInstall\Lock` in a private namespace
//! bounded by the current user's SID:
//!   * No other account can create or open it (its token lacks the SID, and the
//!     descriptor grants only this account and SYSTEM).
//!   * Same-user FUS / RDP sessions still serialise: the namespace is per
//!     machine, not per session.
//!   * The limited and elevated halves of one account open what either created.
//!   * Identity is sourced from the process token, immune to env-var spoof.
//!
//! Acquired by ownership via `WaitForSingleObject`: the
//! `bInitialOwner=TRUE` + `ERROR_ALREADY_EXISTS` idiom does NOT prove
//! acquisition (an existing object's `bInitialOwner` is ignored, and a
//! stale open handle keeps the kernel object alive across retries).
//!
//! Earlier installers, uninstallers and updaters lock `LEGACY_PREFIX` plus the
//! SID instead, and an in-app update never replaces `Uninstall.exe`. The holder
//! of the namespace lock therefore also takes that name, for an earlier build to
//! find, and refuses while an earlier build of this account holds it. Any
//! account can claim a `Global\` name, which rules it out as the lock: a
//! failure on it, or a holder we cannot attribute to us, does not block.
//!
//! Used only by the apply transaction. The `--cleanup-stale` CLI mode
//! deliberately bypasses this: it's a passive helper invoked synchronously
//! by the installer, which already holds the mutex; re-acquiring would
//! deadlock against the caller.

#![cfg(target_os = "windows")]

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::ptr;

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, FALSE, GetLastError, HANDLE, LocalFree,
    WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
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
use windows_sys::Win32::System::Threading::{
    AddSIDToBoundaryDescriptor, ClosePrivateNamespace, CreateBoundaryDescriptorW, CreateMutexW,
    CreatePrivateNamespaceW, DeleteBoundaryDescriptor, GetCurrentProcess, OpenPrivateNamespaceW,
    OpenProcessToken, ReleaseMutex, WaitForSingleObject,
};

/// Named by its boundary: an alias is only this process's prefix for it, and two
/// boundaries of one name and SID are one namespace. Distinct from the app's
/// `TidaLunarApp`, which only the app touches. The installer names it too
/// (`INSTALL_NAMESPACE` in installer/windows/tidalunar.nsi): the two must match.
const NAMESPACE: &str = "TidaLunarInstall";
const MUTEX_LEAF: &str = "Lock";
/// Earlier builds lock this prefix plus the SID. The installer takes the same
/// name (`LEGACY_INSTALL_LOCK` in installer/windows/tidalunar.nsi): the two must
/// match.
const LEGACY_PREFIX: &str = "Global\\TidaLunarInstallLock-";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The current process user: its `TOKEN_USER`, from a buffer the kernel filled
/// for that class, and the string forms an owner check compares.
struct CurrentUser {
    token_user: Vec<u8>,
    sid_text: String,
    /// The owner our token stamps on what it creates, BUILTIN\Administrators
    /// when elevated. `None` when unreadable: only the user SID then counts.
    default_owner: Option<String>,
}

impl CurrentUser {
    /// The SID inside, valid while `self` lives. Read unaligned: a `Vec<u8>`
    /// promises no more.
    fn sid(&self) -> PSID {
        unsafe {
            ptr::read_unaligned(self.token_user.as_ptr().cast::<TOKEN_USER>())
                .User
                .Sid
        }
    }

    /// Exact equality on the `ConvertSidToStringSidW` form: a SID that merely
    /// starts like ours is another account.
    fn owns(&self, owner: &str) -> bool {
        owner == self.sid_text || self.default_owner.as_deref() == Some(owner)
    }
}

/// One token information class, in a buffer sized by the sizing call.
fn token_info(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<u8>> {
    unsafe {
        // The sizing call returns FALSE with ERROR_INSUFFICIENT_BUFFER; only
        // `needed` matters.
        let mut needed: u32 = 0;
        GetTokenInformation(token, class, ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            bail!("GetTokenInformation({class}) returned 0-byte size");
        }
        let mut buf: Vec<u8> = vec![0u8; needed as usize];
        if GetTokenInformation(token, class, buf.as_mut_ptr().cast(), needed, &mut needed) == 0 {
            bail!("GetTokenInformation({class}) failed: {}", GetLastError());
        }
        Ok(buf)
    }
}

/// Fail-closed on the user: any failure returns Err; the caller aborts rather
/// than fall back to a shared name that loses per-user scoping. The default
/// owner only feeds the legacy check, which fails open.
fn current_user() -> Result<CurrentUser> {
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            bail!("OpenProcessToken failed: {}", GetLastError());
        }
        let token_user = token_info(token, TokenUser);
        let owner = token_info(token, TokenOwner);
        CloseHandle(token);
        let token_user = token_user?;
        // Read unaligned: a `Vec<u8>` promises no more.
        let default_owner = owner.ok().and_then(|owner| {
            sid_string(ptr::read_unaligned(owner.as_ptr().cast::<TOKEN_OWNER>()).Owner).ok()
        });
        let sid_text = sid_string(
            ptr::read_unaligned(token_user.as_ptr().cast::<TOKEN_USER>())
                .User
                .Sid,
        )?;
        Ok(CurrentUser {
            token_user,
            sid_text,
            default_owner,
        })
    }
}

/// The SID string of `sid`, the form the descriptor and the owner check use.
fn sid_string(sid: PSID) -> Result<String> {
    unsafe {
        let mut sid_str_ptr: *mut u16 = ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut sid_str_ptr) == 0 {
            bail!("ConvertSidToStringSidW failed: {}", GetLastError());
        }

        let mut len = 0usize;
        while *sid_str_ptr.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(sid_str_ptr, len);
        let s = OsString::from_wide(slice).to_string_lossy().into_owned();
        LocalFree(sid_str_ptr.cast());
        Ok(s)
    }
}

/// The namespace the install mutex lives in, open while this value lives. It
/// outlives its creator while any process holds it, and ends with the last
/// handle. One per process: a process that holds it cannot open it again
/// (`ERROR_DUP_NAME`), so the lock is taken once per process.
struct UserNamespace {
    handle: HANDLE,
    descriptor: PSECURITY_DESCRIPTOR,
}

impl UserNamespace {
    /// Create-or-open, bounded by the SID in `user`.
    fn open(user: &CurrentUser) -> Result<Self> {
        let sid = user.sid();
        let sddl = wide(&format!("D:P(A;;GA;;;{})(A;;GA;;;SY)", user.sid_text));
        let name = wide(NAMESPACE);
        unsafe {
            let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            ) == 0
            {
                bail!("security descriptor rejected: {}", GetLastError());
            }
            let mut boundary = CreateBoundaryDescriptorW(name.as_ptr(), 0);
            if boundary.is_null() {
                let e = GetLastError();
                LocalFree(descriptor.cast());
                bail!("CreateBoundaryDescriptorW failed: {e}");
            }
            let mut handle = ptr::null_mut();
            let mut error = 0;
            if AddSIDToBoundaryDescriptor(&mut boundary, sid) == 0 {
                error = GetLastError();
            } else {
                let attributes = attributes_of(descriptor);
                // One retry covers a namespace its last holder closed between our
                // create and our open.
                for _ in 0..2 {
                    handle = CreatePrivateNamespaceW(&attributes, boundary, name.as_ptr());
                    error = GetLastError();
                    if !handle.is_null() || error != ERROR_ALREADY_EXISTS {
                        break;
                    }
                    handle = OpenPrivateNamespaceW(boundary, name.as_ptr());
                    error = GetLastError();
                    if !handle.is_null() {
                        break;
                    }
                }
            }
            DeleteBoundaryDescriptor(boundary);
            if handle.is_null() {
                LocalFree(descriptor.cast());
                bail!("private namespace unavailable: {error}");
            }
            Ok(Self { handle, descriptor })
        }
    }
}

impl Drop for UserNamespace {
    fn drop(&mut self) {
        // Closed, not destroyed: another holder's objects stay reachable.
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

/// Owned mutex handles, released on Drop (before their namespace) or at process
/// exit. `legacy` is null when the legacy name was unavailable.
pub struct InstallLock {
    handle: HANDLE,
    legacy: HANDLE,
    _namespace: UserNamespace,
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        // Released before closing: a peer holding a handle would otherwise see
        // both owned until this thread exits.
        unsafe {
            for handle in [self.legacy, self.handle] {
                if !handle.is_null() {
                    ReleaseMutex(handle);
                    CloseHandle(handle);
                }
            }
        }
    }
}

/// What the holder of the namespace lock makes of the legacy name.
enum Legacy {
    /// Ours until `InstallLock` drops: an earlier build started now is refused.
    Held(HANDLE),
    /// An earlier build of this account holds it.
    Busy,
    /// Not openable, or held by an owner we cannot attribute to us.
    Unavailable,
}

/// Tries the legacy name without waiting. It is created with the namespace's
/// descriptor: an elevated process would otherwise stamp a default DACL that a
/// limited earlier build cannot open, and that build would report a failure to
/// lock instead of contention.
fn legacy_lock(name: &[u16], attributes: &SECURITY_ATTRIBUTES, user: &CurrentUser) -> Legacy {
    unsafe {
        // Null for an object we may not open or of another type.
        let mutex = CreateMutexW(attributes, FALSE, name.as_ptr());
        if mutex.is_null() {
            return Legacy::Unavailable;
        }
        let wait = WaitForSingleObject(mutex, 0);
        if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
            return Legacy::Held(mutex);
        }
        // An earlier build creates it with its token's default owner, which
        // `owns` accepts. Any account can hold a `Global\` name; only an
        // administrator can give it an owner that `owns` accepts.
        let busy = wait == WAIT_TIMEOUT && owner_sid(mutex).is_some_and(|owner| user.owns(&owner));
        CloseHandle(mutex);
        if busy {
            Legacy::Busy
        } else {
            Legacy::Unavailable
        }
    }
}

/// The owner the kernel keeps on the object behind `handle`. Needs READ_CONTROL,
/// which `CreateMutexW`'s MUTEX_ALL_ACCESS carries.
fn owner_sid(handle: HANDLE) -> Option<String> {
    let mut owner: PSID = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    unsafe {
        let rc = GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        );
        if rc != ERROR_SUCCESS {
            return None;
        }
        let sid = sid_string(owner).ok();
        LocalFree(descriptor.cast());
        sid
    }
}

/// Try to acquire the install/update mutex.
/// Returns:
///   * `Ok(Some(lock))` - owned (WAIT_OBJECT_0 or WAIT_ABANDONED).
///   * `Ok(None)` - contention; another installer/updater holds it, or an
///     earlier build holds the legacy name. Caller should bail with a
///     user-facing message.
///   * `Err(_)` - hard failure (token / SID / namespace error -> fail closed).
pub fn try_acquire() -> Result<Option<InstallLock>> {
    acquire(LEGACY_PREFIX)
}

/// `try_acquire`, with the legacy name built from `legacy_prefix`.
fn acquire(legacy_prefix: &str) -> Result<Option<InstallLock>> {
    let user = current_user().context("retrieve current user SID")?;
    let namespace = UserNamespace::open(&user).context("open the install namespace")?;
    let attributes = attributes_of(namespace.descriptor);
    let name = wide(&format!("{NAMESPACE}\\{MUTEX_LEAF}"));
    let legacy_name = wide(&format!("{legacy_prefix}{}", user.sid_text));

    unsafe {
        let h = CreateMutexW(&attributes, FALSE, name.as_ptr());
        if h.is_null() {
            bail!("CreateMutexW failed: {}", GetLastError());
        }
        let r = WaitForSingleObject(h, 0);
        if r != WAIT_OBJECT_0 && r != WAIT_ABANDONED {
            // Contention or wait error: release our handle, letting the kernel
            // reap the object once the real owner exits.
            CloseHandle(h);
            return Ok(None);
        }
        let legacy = match legacy_lock(&legacy_name, &attributes, &user) {
            Legacy::Held(legacy) => legacy,
            Legacy::Unavailable => ptr::null_mut(),
            Legacy::Busy => {
                // Owned by this thread, which stays alive through the caller's
                // dialogs: a close alone would leave a peer's wait timing out.
                ReleaseMutex(h);
                CloseHandle(h);
                return Ok(None);
            }
        };
        Ok(Some(InstallLock {
            handle: h,
            legacy,
            _namespace: namespace,
        }))
    }
}

#[cfg(test)]
#[path = "../tests/unit/lock/lock_tests.rs"]
mod lock_tests;
