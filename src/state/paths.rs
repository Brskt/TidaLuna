#[cfg(test)]
use super::test_root;
use std::path::{Path, PathBuf};
#[cfg(not(test))]
use std::sync::OnceLock;

/// `env` when it names an absolute directory, else `fallback`'s answer when that does.
///
/// A relative directory is no directory at all here: it follows whatever the
/// current directory is, and CEF refuses one outright, running its profile in
/// memory.
///
/// A base that is not UTF-8 text is refused rather than replaced. CEF and the
/// native host only take paths as text: a lossy copy would send them to a
/// sibling directory while the rest of the app stays on the real one; falling
/// back would instead move the data somewhere the user never named.
pub(crate) fn resolve_base(
    env: Option<std::ffi::OsString>,
    fallback: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    env.map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| fallback().filter(|dir| dir.is_absolute()))
        .filter(|dir| dir.to_str().is_some())
}

/// What the running executable's path gives CEF to load its resources from.
#[cfg(not(target_os = "macos"))]
#[derive(Debug, PartialEq)]
pub(crate) enum InstallDir {
    /// The directory holding the executable.
    Found(PathBuf),
    /// The system cannot name the executable; CEF looks on its own.
    Unknown,
    /// The path is not UTF-8 text, which CEF's resource settings cannot carry.
    NotText,
}

/// Judges the whole path rather than its directory, since the desktop entry and
/// the `tidal://` handler write the executable itself out as text.
#[cfg(not(target_os = "macos"))]
pub(crate) fn install_dir(exe: std::io::Result<PathBuf>) -> InstallDir {
    match exe {
        Err(_) => InstallDir::Unknown,
        Ok(exe) if exe.to_str().is_none() => InstallDir::NotText,
        Ok(exe) => exe.parent().map_or(InstallDir::Unknown, |dir| {
            InstallDir::Found(dir.to_path_buf())
        }),
    }
}

/// The account's home directory, resolved once. `home_dir` reads `HOME` again,
/// then the account database when `HOME` is unset or empty.
#[cfg(all(not(test), not(target_os = "windows")))]
pub(crate) fn user_home() -> Option<&'static Path> {
    static HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
    HOME.get_or_init(|| resolve_base(std::env::var_os("HOME"), std::env::home_dir))
        .as_deref()
}

/// The account's home folder, what a folder grant may never be or hold. `None` when it cannot be
/// named, and then no folder is granted at all.
#[cfg(all(not(test), not(target_os = "windows")))]
pub(crate) fn home_folder() -> Option<&'static Path> {
    user_home()
}

/// The profile folder, from `USERPROFILE` first as the shell sets it, then as the shell knows it.
#[cfg(all(not(test), target_os = "windows"))]
pub(crate) fn home_folder() -> Option<&'static Path> {
    static HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
    HOME.get_or_init(|| {
        resolve_base(std::env::var_os("USERPROFILE"), || {
            known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Profile)
        })
    })
    .as_deref()
}

/// Under test the home is the process's own directory, never the developer's.
#[cfg(test)]
pub(crate) fn home_folder() -> Option<&'static Path> {
    Some(&test_root::root().home)
}

/// A known folder as the shell names it, for an environment variable that is missing, empty or
/// relative. Callers read the variable first: the data has always lived where it points.
#[cfg(target_os = "windows")]
fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath};

    let mut raw: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: the folder id outlives the call, a null token names the current
    // user, and `raw` receives a string this function frees on either outcome,
    // as the API requires.
    let hr = unsafe {
        SHGetKnownFolderPath(
            id,
            KF_FLAG_DONT_VERIFY as u32,
            std::ptr::null_mut(),
            &mut raw,
        )
    };
    let path = (hr >= 0 && !raw.is_null()).then(|| {
        // SAFETY: on success `raw` is a NUL-terminated wide string the API owns
        // until the free below.
        let wide = unsafe {
            let len = (0..).take_while(|&i| *raw.add(i) != 0).count();
            std::slice::from_raw_parts(raw, len)
        };
        PathBuf::from(std::ffi::OsString::from_wide(wide))
    });
    // SAFETY: `raw` came from `SHGetKnownFolderPath`; freeing null is a no-op.
    unsafe { CoTaskMemFree(raw.cast()) };
    path
}

/// The folder this app keeps under each per-user base.
pub(super) const APP_DIR: &str = "tidalunar";

/// Where this account's data lives, resolved once; `None` when no absolute
/// UTF-8 directory can be named, and then `main` refuses to start.
#[cfg(not(test))]
pub(crate) fn data_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        #[cfg(target_os = "windows")]
        {
            resolve_base(std::env::var_os("LOCALAPPDATA"), || {
                known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData)
            })
            .map(|base| base.join(APP_DIR))
        }
        #[cfg(not(target_os = "windows"))]
        {
            user_home().map(|home| home.join(".local").join("share").join(APP_DIR))
        }
    })
    .as_deref()
}

/// Under test, every per-user path lands in this process's own directory; the account's real
/// one holds the developer's cache, settings and tokens, and a test that opens the audio cache
/// can evict from it.
#[cfg(test)]
pub(crate) fn data_dir() -> Option<&'static Path> {
    Some(&test_root::root().data)
}

/// Only the Linux desktop entry asks for the home under test.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn user_home() -> Option<&'static Path> {
    Some(&test_root::root().home)
}

/// The data directory. Every caller runs after `main` has refused to start
/// without one; only the log sink asks earlier, through `data_dir`.
pub fn cache_data_dir() -> PathBuf {
    data_dir()
        .expect("main refuses to start without a data directory")
        .to_owned()
}

#[cfg(test)]
#[path = "../../tests/unit/state/paths.rs"]
mod tests;
