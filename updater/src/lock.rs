//! Win32 install/update mutex, `TidaLunarInstall\Lock` in a private namespace
//! bounded by the current user's SID:
//!   * No other account can create or open it (its token lacks the SID, and the
//!     descriptor grants only this account and SYSTEM).
//!   * Same-user FUS / RDP sessions still serialise: the namespace is per
//!     machine, not per session.
//!   * The limited and elevated halves of one account open what either created.
//!   * Identity is sourced from the process token, immune to env-var spoof.
//!
//! Acquired by ownership via `WaitForSingleObject` - the
//! `bInitialOwner=TRUE` + `ERROR_ALREADY_EXISTS` idiom does NOT prove
//! acquisition (an existing object's `bInitialOwner` is ignored, and a
//! stale open handle keeps the kernel object alive across retries).
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
    CloseHandle, ERROR_ALREADY_EXISTS, FALSE, GetLastError, HANDLE, LocalFree, WAIT_ABANDONED,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::System::Threading::{
    AddSIDToBoundaryDescriptor, ClosePrivateNamespace, CreateBoundaryDescriptorW, CreateMutexW,
    CreatePrivateNamespaceW, DeleteBoundaryDescriptor, GetCurrentProcess, OpenPrivateNamespaceW,
    OpenProcessToken, WaitForSingleObject,
};

/// Named by its boundary: an alias is only this process's prefix for it, and two
/// boundaries of one name and SID are one namespace. Distinct from the app's
/// `TidaLunarApp`, which only the app touches. The installer names it too
/// (`INSTALL_NAMESPACE` in installer/windows/tidalunar.nsi): the two must match.
const NAMESPACE: &str = "TidaLunarInstall";
const MUTEX_LEAF: &str = "Lock";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The current process user's `TOKEN_USER`. Only `current_user` builds one, from
/// a buffer the kernel filled for that class.
struct CurrentUser(Vec<u8>);

impl CurrentUser {
    /// The SID inside, valid while `self` lives. Read unaligned: a `Vec<u8>`
    /// promises no more.
    fn sid(&self) -> PSID {
        unsafe {
            ptr::read_unaligned(self.0.as_ptr().cast::<TOKEN_USER>())
                .User
                .Sid
        }
    }
}

/// Fail-closed: any failure returns Err; the caller aborts rather than fall back
/// to a shared name that loses per-user scoping.
fn current_user() -> Result<CurrentUser> {
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            bail!("OpenProcessToken failed: {}", GetLastError());
        }

        // First call sizes the buffer (returns FALSE with ERROR_INSUFFICIENT_BUFFER
        // - that's expected here, we only care that `needed` is populated).
        let mut needed: u32 = 0;
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            CloseHandle(token);
            bail!("GetTokenInformation returned 0-byte size");
        }

        let mut buf: Vec<u8> = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        );
        if ok == 0 {
            let e = GetLastError();
            CloseHandle(token);
            bail!("GetTokenInformation(TokenUser) failed: {e}");
        }
        CloseHandle(token);
        Ok(CurrentUser(buf))
    }
}

/// The SID string of `sid`, for the descriptor.
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
        let sid_text = sid_string(sid)?;
        let sddl = wide(&format!("D:P(A;;GA;;;{sid_text})(A;;GA;;;SY)"));
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

/// Owned mutex handle, released on Drop (before its namespace) or at process exit.
pub struct InstallLock {
    handle: HANDLE,
    _namespace: UserNamespace,
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        unsafe {
            if !self.handle.is_null() {
                CloseHandle(self.handle);
            }
        }
    }
}

/// Try to acquire the install/update mutex.
/// Returns:
///   * `Ok(Some(lock))` - owned (WAIT_OBJECT_0 or WAIT_ABANDONED).
///   * `Ok(None)` - contention; another installer/updater holds it. Caller
///     should bail with a user-facing message.
///   * `Err(_)` - hard failure (token / SID / namespace error -> fail closed).
pub fn try_acquire() -> Result<Option<InstallLock>> {
    let user = current_user().context("retrieve current user SID")?;
    let namespace = UserNamespace::open(&user).context("open the install namespace")?;
    let attributes = attributes_of(namespace.descriptor);
    let name = wide(&format!("{NAMESPACE}\\{MUTEX_LEAF}"));

    unsafe {
        let h = CreateMutexW(&attributes, FALSE, name.as_ptr());
        if h.is_null() {
            bail!("CreateMutexW failed: {}", GetLastError());
        }
        let r = WaitForSingleObject(h, 0);
        if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
            return Ok(Some(InstallLock {
                handle: h,
                _namespace: namespace,
            }));
        }
        // Contention or wait error: release our handle, letting the kernel
        // reap the object once the real owner exits.
        CloseHandle(h);
        Ok(None)
    }
}

#[cfg(test)]
#[path = "../tests/unit/lock/lock_tests.rs"]
mod lock_tests;
