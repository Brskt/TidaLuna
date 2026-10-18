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

#[cfg(test)]
#[path = "../../tests/unit/platform/process_args.rs"]
mod tests;
