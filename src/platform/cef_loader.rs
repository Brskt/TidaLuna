//! The macOS bootstrap for CEF, and the only code that gets to run before the
//! first `cef_*` call.
//!
//! Two jobs, in an order that is not ours to choose. A subprocess has to adopt
//! Chromium's sandbox context first, because Chromium verifies that adoption and
//! aborts the process when it is missing. Then the framework has to be loaded:
//! macOS resolves it at launch instead of linking it, so every `cef_*` entry
//! point stays a null pointer until `cef_load_library` fills the dispatch table.
//! Windows points the OS loader at `bin/cef` and Linux leans on an rpath; this is
//! the macOS half of the same job.

use std::ffi::{CString, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Hop from the main executable's directory up to the frameworks directory:
/// `Contents/MacOS/<exe>` to `Contents/Frameworks`.
const FROM_MAIN_EXE: &str = "../Frameworks";

/// A helper executable sits one `.app` deeper and climbs back out:
/// `Contents/Frameworks/<n>.app/Contents/MacOS/<n>` to `Contents/Frameworks`.
const FROM_HELPER_EXE: &str = "../../..";

/// The framework's own binary, which is the file the loader opens.
const FRAMEWORK_BINARY: &str = "Chromium Embedded Framework.framework/Chromium Embedded Framework";

/// The sandbox library, which only a subprocess ever opens.
const SANDBOX_DYLIB: &str = "Chromium Embedded Framework.framework/Libraries/libcef_sandbox.dylib";

/// What a subprocess keeps alive for as long as it runs.
///
/// Field order carries no invariant, contrary to what an earlier revision of this
/// comment claimed. The context does not retain the argument vector: CEF's
/// `sandbox_mac.mm` hands `argc`/`argv` to `SeatbeltExecServer::CreateFromArguments`,
/// which keeps a single `int fd_`, and `cef_sandbox_destroy` takes the context and
/// nothing else. Both fields are held only because a subprocess has no reason to
/// release either before it exits.
pub(crate) struct SandboxGuard {
    _sandbox: cef::sandbox::Sandbox,
    _args: cef::args::Args,
}

/// True when Chromium launched this executable as one of its subprocesses.
///
/// The switch is read from the raw arguments because the CEF command-line API is
/// itself part of the library that has not been loaded yet. It is read as bytes
/// rather than as `String` because the UTF-8 flavour unwraps internally: one
/// argument that is not valid Unicode would panic here, and a panic at this point
/// has no console, no sink and no `verr!` in front of it.
fn is_helper_process(mut args: impl Iterator<Item = OsString>) -> bool {
    args.any(|a| a.as_bytes().starts_with(b"--type="))
}

/// Where the framework binary sits, seen from the directory holding the running
/// executable.
///
/// A helper executable lives one `.app` deeper than the main one, so the two
/// climb different distances to reach the same `Contents/Frameworks`.
fn framework_path(exe_dir: &Path, is_helper: bool) -> PathBuf {
    let resolver = if is_helper {
        FROM_HELPER_EXE
    } else {
        FROM_MAIN_EXE
    };
    exe_dir.join(resolver).join(FRAMEWORK_BINARY)
}

/// Where the framework sits in a tree that was built but never bundled.
///
/// The hops above describe the inside of a `.app`. `cargo run` and every test binary live
/// under `target/` and have none of that layout; they had no way to find the framework at
/// all, and the first `cef_*` call jumped through a null dispatch entry.
///
/// The path is baked at compile time by `build.rs`, out of the metadata `cef-dll-sys`
/// publishes, because the framework's real home carries that crate's build-script hash and
/// changes on every rebuild: there is nothing stable to search for at runtime.
fn build_tree_framework_path() -> Option<PathBuf> {
    #[cfg(has_cef_build_dir)]
    {
        Some(Path::new(env!("CEF_BUILD_FRAMEWORK_DIR")).join(FRAMEWORK_BINARY))
    }
    #[cfg(not(has_cef_build_dir))]
    {
        None
    }
}

/// Where the sandbox library sits, seen from a subprocess. The main process never
/// opens it, so only the helper hop exists.
fn sandbox_dylib_path(exe_dir: &Path) -> PathBuf {
    exe_dir.join(FROM_HELPER_EXE).join(SANDBOX_DYLIB)
}

/// The text a caught panic carried, if it carried one.
///
/// `panic!` and `assert!` box a `&str` or a `String` depending on whether the
/// message was formatted, and `unwrap` always formats, so both shapes have to be
/// tried before giving up.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("no message")
}

/// Adopt Chromium's sandbox context, or terminate the process.
///
/// Every failure this can meet arrives as a panic rather than a `Result`: the
/// crate's `Sandbox::new` unwraps the executable path, its parent, the
/// canonicalised dylib path and the `dlopen`, and `initialize` unwraps a symbol
/// lookup and then asserts the context is non-null. Seven ways to abort, none of
/// them reportable, in the one process that has no console to abort into.
///
/// Upstream is not a way out: `sandbox.rs` is byte-identical in the newest
/// release (151.8.0). Catching is sound here because all seven are raised in Rust
/// frames after the FFI calls have returned, so no unwind crosses a C boundary.
fn enter_sandbox(exe_dir: &Path) -> SandboxGuard {
    let adopted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let args = cef::args::Args::new();
        let mut sandbox = cef::sandbox::Sandbox::new();
        sandbox.initialize(args.as_main_args());
        SandboxGuard {
            _sandbox: sandbox,
            _args: args,
        }
    }));

    match adopted {
        Ok(guard) => {
            crate::vprintln!("[CEF]    Sandbox context adopted");
            guard
        }
        Err(payload) => {
            // Nothing built survives a caught panic here, so the state is read off
            // the filesystem instead: it separates an absent library from one that
            // is there and refuses to load, which are different bundle problems.
            let dylib = sandbox_dylib_path(exe_dir);
            let state = if dylib.exists() {
                "will not load"
            } else {
                "is missing"
            };
            // The panic message names the underlying failure, and the default hook
            // writes it to a stderr a subprocess does not have. Carrying it over
            // keeps the whole diagnosis on the one channel that survives.
            let cause = panic_message(payload.as_ref());

            crate::verr!("[CEF]    Sandbox library {state}: {}", dylib.display());
            crate::verr!("[CEF]    Cause: {cause}");
            crate::verr!("[CEF]    The .app bundle is incomplete; unpack the release again.");
            std::process::exit(1);
        }
    }
}

/// Load the CEF framework, or terminate the process.
///
/// No recoverable branch exists here: returning after a failure would leave every
/// later `cef_*` call jumping through a null pointer.
fn load_framework(exe_dir: &Path, is_helper: bool) {
    let candidate = framework_path(exe_dir, is_helper);

    // canonicalize() doubles as the existence check and strips the `..` hops, which
    // keeps the failure message readable.
    //
    // The bundle is tried FIRST and wins wherever it exists: a shipped app never reaches
    // for a path baked by the machine that built it. The fallback answers only for the trees
    // that have no bundle at all.
    let framework = match candidate.canonicalize() {
        Ok(path) => path,
        Err(e) => match build_tree_framework_path().and_then(|p| p.canonicalize().ok()) {
            Some(path) => path,
            None => {
                crate::verr!("[CEF]    No framework at {}: {e}", candidate.display());
                crate::verr!("[CEF]    The .app bundle is incomplete; unpack the release again.");
                std::process::exit(1);
            }
        },
    };

    let Ok(path) = CString::new(framework.as_os_str().as_bytes()) else {
        crate::verr!(
            "[CEF]    Framework path {} holds a NUL byte.",
            framework.display()
        );
        std::process::exit(1);
    };

    // SAFETY: as_ptr() yields a live `*const c_char` that `path` owns for the whole
    // call, and CString guarantees the NUL terminator the C API reads past it.
    if unsafe { cef::load_library(Some(&*path.as_ptr())) } != 1 {
        crate::verr!(
            "[CEF]    Framework at {} refused to load.",
            framework.display()
        );
        std::process::exit(1);
    }

    // Deliberately never unloaded: the framework has to outlive every CEF call,
    // and process exit reclaims it anyway.
    crate::vprintln!("[CEF]    Framework loaded from {}", framework.display());
}

/// Prepare this process to call into CEF, or terminate it.
///
/// Chromium spawns its renderer, GPU and utility processes from copies of this
/// same executable, so one binary answers for both roles and reads which one it
/// is from `--type=`. The returned guard is `Some` only in a subprocess, and the
/// caller has to hold it for the rest of the process.
pub(crate) fn bootstrap() -> Option<SandboxGuard> {
    let exe_dir = running_exe_dir();
    let is_helper = is_helper_process(std::env::args_os());
    let guard = is_helper.then(|| enter_sandbox(&exe_dir));
    ensure_framework_loaded();
    guard
}

/// The directory holding the running executable, or the end of the process.
///
/// Both callers need it before any `cef_*` exists to report with, which is why the failures
/// terminate here rather than travel back as a `Result` nobody could act on.
fn running_exe_dir() -> PathBuf {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            crate::verr!("[CEF]    Cannot locate the running executable: {e}");
            std::process::exit(1);
        }
    };
    match exe.parent() {
        Some(dir) => dir.to_path_buf(),
        None => {
            crate::verr!(
                "[CEF]    Executable {} has no parent directory.",
                exe.display()
            );
            std::process::exit(1);
        }
    }
}

/// Fill CEF's dispatch table once per process, whoever asks.
///
/// `bootstrap()` is the production caller and runs exactly once, from `main`. A test binary
/// has no `main`; each test that reaches a `cef_*` call asks here instead, and they must
/// not stack `cef::load_library` calls on one another, hence the `Once`.
pub(crate) fn ensure_framework_loaded() {
    static LOADED: std::sync::Once = std::sync::Once::new();
    LOADED.call_once(|| {
        load_framework(&running_exe_dir(), is_helper_process(std::env::args_os()));
    });
}

#[cfg(test)]
#[path = "../../tests/unit/platform/cef_loader.rs"]
mod tests;
