#[cfg(not(target_os = "windows"))]
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::platform::EXE_NAME;

#[cfg(target_os = "windows")]
pub(crate) fn wait_for_pid(pid: u32) -> Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    const WAIT_OBJECT_0: u32 = 0x00000000;
    const WAIT_TIMEOUT: u32 = 0x00000102;

    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            // Process already gone
            return Ok(());
        }
        // Wait up to 30 seconds
        let result = WaitForSingleObject(handle, 30_000);
        CloseHandle(handle);
        match result {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => bail!("timeout waiting for PID {pid} to exit"),
            _ => bail!("WaitForSingleObject failed for PID {pid} (code {result:#010x})"),
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn wait_for_pid(pid: u32) -> Result<()> {
    use std::ffi::c_int;
    unsafe extern "C" {
        fn kill(pid: c_int, sig: c_int) -> c_int;
    }

    for _ in 0..60 {
        // signal 0 = check if process exists
        if unsafe { kill(pid as c_int, 0) } != 0 {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(500));
    }
    bail!("timeout waiting for PID {pid} to exit");
}

pub(crate) fn probe_exclusive_access(app_dir: &Path) -> Result<()> {
    // Check the exe at root
    let mut paths_to_check: Vec<std::path::PathBuf> = vec![app_dir.join(EXE_NAME)];

    // Check CEF DLLs in both old (root) and new (bin/cef/) layouts
    let cef_libs: &[&str] = if cfg!(target_os = "windows") {
        &[
            "libcef.dll",
            "chrome_elf.dll",
            "libEGL.dll",
            "libGLESv2.dll",
        ]
    } else {
        &["libcef.so"]
    };
    for lib in cef_libs {
        for subdir in &["", "cef", "bin/cef"] {
            paths_to_check.push(app_dir.join(subdir).join(lib));
        }
    }

    // Check bun in both old (root) and new (bin/) layouts
    let bun_name = if cfg!(target_os = "windows") {
        "bun.exe"
    } else {
        "bun"
    };
    paths_to_check.push(app_dir.join(bun_name));
    paths_to_check.push(app_dir.join("bin").join(bun_name));

    for path in &paths_to_check {
        if !path.exists() {
            continue;
        }

        let mut locked = true;
        for attempt in 1..=3 {
            if try_exclusive_access(path)? {
                locked = false;
                break;
            }
            let display = path.file_name().unwrap_or_default().to_string_lossy();
            say!("[updater] {display} is locked, retry {attempt}/3...");
            thread::sleep(Duration::from_secs(2));
        }

        if locked {
            let display = path.file_name().unwrap_or_default().to_string_lossy();
            bail!("{display} is still locked by another process - cannot update");
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn try_exclusive_access(path: &Path) -> Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            0, // no sharing
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        Ok(false) // locked
    } else {
        unsafe { CloseHandle(handle) };
        Ok(true) // accessible
    }
}

#[cfg(not(target_os = "windows"))]
fn try_exclusive_access(path: &Path) -> Result<bool> {
    use std::os::unix::io::AsRawFd;

    let file = match fs::OpenOptions::new().write(true).open(path) {
        Ok(f) => f,
        Err(_) => return Ok(false),
    };

    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }

    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    const LOCK_UN: i32 = 8;

    let fd = file.as_raw_fd();
    let result = unsafe { flock(fd, LOCK_EX | LOCK_NB) };
    if result == 0 {
        // Got the lock - unlock and report accessible
        unsafe { flock(fd, LOCK_UN) };
        Ok(true)
    } else {
        Ok(false)
    }
}

pub(crate) fn relaunch(app_dir: &Path) -> Result<()> {
    let exe = app_dir.join(EXE_NAME);

    let mut cmd = Command::new(&exe);
    cmd.current_dir(app_dir);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW - app will create its own
    }

    cmd.spawn()
        .with_context(|| format!("failed to relaunch {}", exe.display()))?;

    Ok(())
}

#[cfg(target_os = "windows")]
pub(crate) fn show_error(msg: &str) {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    let wide_msg: Vec<u16> = OsStr::new(msg).encode_wide().chain(Some(0)).collect();
    let wide_title: Vec<u16> = OsStr::new("TidaLunar Update Error")
        .encode_wide()
        .chain(Some(0))
        .collect();

    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
            std::ptr::null_mut(),
            wide_msg.as_ptr(),
            wide_title.as_ptr(),
            windows_sys::Win32::UI::WindowsAndMessaging::MB_OK
                | windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
        );
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn show_error(msg: &str) {
    say!("ERROR: {msg}");
}
