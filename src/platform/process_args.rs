//! This process's arguments as CEF takes them, built without `std::env::args()`.
//!
//! The `cef` crate's `Args` reads `std::env::args()`, which panics on an
//! argument that is not valid Unicode: a stray byte beside a `tidal://` link, or
//! the executable's own path in every subprocess Chromium starts from it. Upstream
//! cef-rs still reads it that way, on master and in every 152.x and 154.x release.

#[cfg(not(target_os = "windows"))]
use std::ffi::{OsString, c_char, c_int};

use cef::{CommandLine, ImplCommandLine, MainArgs, command_line_create};

/// The arguments of this process, owned for as long as CEF may read them.
pub(crate) struct ProcessArgs {
    /// One NUL-terminated buffer per argument, owned and writable: Chromium
    /// rewrites argv in place, which is also why CEF's own sample hands it a copy.
    #[cfg(not(target_os = "windows"))]
    _buffers: Vec<Vec<u8>>,
    #[cfg(not(target_os = "windows"))]
    _argv: Vec<*mut c_char>,
    main_args: MainArgs,
}

impl ProcessArgs {
    #[cfg(not(target_os = "windows"))]
    pub(crate) fn new() -> Self {
        let mut buffers = argv_buffers(std::env::args_os());
        let mut argv: Vec<*mut c_char> = buffers
            .iter_mut()
            .map(|arg| arg.as_mut_ptr().cast())
            .collect();
        let argc = argv.len() as c_int;
        // `argv[argc]` is null in a real `main`, and C code may walk to it.
        argv.push(std::ptr::null_mut());
        let main_args = MainArgs {
            argc,
            argv: argv.as_mut_ptr(),
        };
        // Moving the vectors keeps their heap storage where the pointers point.
        Self {
            _buffers: buffers,
            _argv: argv,
            main_args,
        }
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn new() -> Self {
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        // SAFETY: a null name asks for the module that started this process.
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        Self {
            main_args: MainArgs {
                instance: cef::sys::HINSTANCE(instance.cast()),
            },
        }
    }

    pub(crate) fn as_main_args(&self) -> &MainArgs {
        &self.main_args
    }

    #[cfg(not(target_os = "windows"))]
    pub(crate) fn as_cmd_line(&self) -> Option<CommandLine> {
        let cmd_line = command_line_create()?;
        cmd_line.init_from_argv(self.main_args.argc, self.main_args.argv.cast());
        Some(cmd_line)
    }

    /// Windows hands the line over as UTF-16, which CEF reads as it is: no
    /// conversion stands between the two that could fail.
    #[cfg(target_os = "windows")]
    pub(crate) fn as_cmd_line(&self) -> Option<CommandLine> {
        let cmd_line = command_line_create()?;
        let line = cef::CefString::from(borrowed_utf16(command_line_utf16()));
        cmd_line.init_from_string(Some(&line));
        Some(cmd_line)
    }
}

/// True when Chromium launched this executable as one of its subprocesses.
///
/// The switch is read from the raw arguments because the CEF command-line API is
/// itself part of the library, which on macOS has not been loaded yet when this is
/// first needed. It is read as bytes rather than as `String` because the UTF-8
/// flavour unwraps internally: one argument that is not valid Unicode would panic
/// here, and a panic at this point has no console, no sink and no `verr!` in front
/// of it. The browser process never carries `--type=`; CEF's own sample classifies
/// its processes the same way.
pub(crate) fn is_helper_process(mut args: impl Iterator<Item = std::ffi::OsString>) -> bool {
    args.any(|a| a.as_encoded_bytes().starts_with(b"--type="))
}

/// Each argument's bytes as they arrived, NUL-terminated. An argument cannot
/// hold a NUL of its own, being a C string to begin with.
#[cfg(not(target_os = "windows"))]
fn argv_buffers(args: impl IntoIterator<Item = OsString>) -> Vec<Vec<u8>> {
    use std::os::unix::ffi::OsStringExt;
    args.into_iter()
        .map(|arg| {
            let mut bytes = arg.into_vec();
            bytes.push(0);
            bytes
        })
        .collect()
}

/// The command line the system keeps for this process, without its NUL.
#[cfg(target_os = "windows")]
fn command_line_utf16() -> &'static [u16] {
    use windows_sys::Win32::System::Environment::GetCommandLineW;
    // SAFETY: the system owns this NUL-terminated string for the life of the
    // process and never frees it.
    unsafe {
        let line = GetCommandLineW();
        if line.is_null() {
            return &[];
        }
        let len = (0..).take_while(|&i| *line.add(i) != 0).count();
        std::slice::from_raw_parts(line, len)
    }
}

/// A CEF string over `wide` that copies nothing and frees nothing, sound because
/// the text outlives every reader.
#[cfg(target_os = "windows")]
fn borrowed_utf16(wide: &'static [u16]) -> cef::sys::_cef_string_utf16_t {
    cef::sys::_cef_string_utf16_t {
        str_: wide.as_ptr().cast_mut(),
        length: wide.len(),
        dtor: None,
    }
}

#[cfg(test)]
#[path = "../../tests/unit/platform/process_args.rs"]
mod tests;
