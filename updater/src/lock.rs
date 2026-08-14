//! Win32 install/update mutex. Per-user (SID-scoped, NOT %USERNAME%) via
//! `Global\TidaLunarInstallLock-<SID>` so:
//!   * Cross-user contention is avoided (different SID = different mutex).
//!   * Same-user FUS / RDP sessions still serialise (`Global\` namespace).
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
    CloseHandle, FALSE, GetLastError, HANDLE, LocalFree, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, OpenProcessToken, WaitForSingleObject,
};

const MUTEX_NAME_PREFIX: &str = "Global\\TidaLunarInstallLock-";

/// Retrieve the current process user's SID as a string. Fail-closed: any
/// failure returns Err; the caller aborts rather than fall back to a
/// shared name that loses per-user scoping.
fn current_user_sid_string() -> Result<String> {
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

        let token_user = buf.as_ptr().cast::<TOKEN_USER>();
        let sid_ptr = (*token_user).User.Sid;

        let mut sid_str_ptr: *mut u16 = ptr::null_mut();
        if ConvertSidToStringSidW(sid_ptr, &mut sid_str_ptr) == 0 {
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

/// Owned mutex handle - released on Drop or process exit.
pub struct InstallLock {
    handle: HANDLE,
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
///   * `Err(_)` - hard failure (token / SID lookup error → fail closed).
pub fn try_acquire() -> Result<Option<InstallLock>> {
    let sid = current_user_sid_string().context("retrieve current user SID")?;
    let name = format!("{MUTEX_NAME_PREFIX}{sid}");
    let mut wide: Vec<u16> = name.encode_utf16().collect();
    wide.push(0);

    unsafe {
        let h = CreateMutexW(ptr::null(), FALSE, wide.as_ptr());
        if h.is_null() {
            bail!("CreateMutexW failed: {}", GetLastError());
        }
        let r = WaitForSingleObject(h, 0);
        if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
            return Ok(Some(InstallLock { handle: h }));
        }
        // Contention or wait error: release our handle, letting the kernel
        // reap the object once the real owner exits.
        CloseHandle(h);
        Ok(None)
    }
}
