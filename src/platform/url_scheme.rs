//! Windows: claim the `tidal://` scheme for this executable.
//!
//! Written at runtime rather than by the installer, so a portable copy is
//! registered too and the recorded command follows the executable when it
//! moves. The installer is per-user (`RequestExecutionLevel user`), so the keys
//! go under `HKEY_CURRENT_USER` to match; the per-machine hive would need admin
//! and would claim the scheme for other accounts.
#![cfg(target_os = "windows")]

use std::path::Path;

use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegSetValueExW,
};

use crate::platform::wide;
use crate::ui::deep_link::SCHEME;

/// A URL carrying a double quote breaks out of `"%1"` and appends its own tokens
/// to the line CEF reads through `GetCommandLineW`. Past a bare `--` Chromium
/// stops reading tokens as switches, leaving what the shell split out a positional
/// argument rather than `--browser-subprocess-path`.
///
/// Named rather than inlined because the uninstaller compares against this exact
/// shape to tell our key from another client's, and cannot see this function; a test
/// holds the two together.
fn open_command(exe: &Path) -> String {
    format!("\"{}\" -- \"%1\"", exe.display())
}

/// Browser process only, like the Linux desktop entry, since CEF subprocesses
/// re-enter `main()` and would each rewrite the same keys.
pub(crate) fn register() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let command = open_command(&exe);

    // `URL Protocol` is the marker the shell checks before treating the key as a
    // protocol handler at all; without it the rest is an ordinary file ProgID and
    // a `tidal://` click still fails.
    let root = format!("Software\\Classes\\{SCHEME}");
    write_value(&root, "", &format!("URL:{SCHEME} Protocol"));
    write_value(&root, "URL Protocol", "");
    write_value(&format!("{root}\\shell\\open\\command"), "", &command);
}

fn write_value(subkey: &str, name: &str, value: &str) {
    let subkey = wide(subkey);
    let mut hkey: HKEY = std::ptr::null_mut();
    // SAFETY: subkey is NUL-terminated UTF-16; hkey receives an owned key on success.
    let opened = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            std::ptr::null(),
            0,
            KEY_WRITE,
            std::ptr::null(),
            &mut hkey,
            std::ptr::null_mut(),
        )
    };
    if opened != 0 {
        crate::vprintln!("[SCHEME] could not open the registry key for {SCHEME}");
        return;
    }

    let name = wide(name);
    let value = wide(value);
    // SAFETY: hkey is live; both buffers outlive the call and the length counts
    // their NUL, which REG_SZ readers expect.
    unsafe {
        RegSetValueExW(
            hkey,
            name.as_ptr(),
            0,
            REG_SZ,
            value.as_ptr().cast(),
            (value.len() * 2) as u32,
        );
        RegCloseKey(hkey);
    }
}

#[cfg(test)]
#[path = "../../tests/unit/platform/url_scheme.rs"]
mod tests;
