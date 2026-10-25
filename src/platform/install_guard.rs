//! The app's own files, which no plugin may write.
//!
//! Rewriting `frontend/native-host.cjs` would lift the native sandbox, and the unsigned
//! `.update-journal.json` drives boot renames. The whole install folder is refused, not a list
//! (Windows loads exe-folder DLLs first), judged by file identity, not path spelling.
//!
//! Only the folder holding the native plugins' data dirs is left out: on Linux the deb unpacks the
//! app into the data dir, and native plugins write their own dataDir there.

use std::path::{Component, Path};
use std::sync::OnceLock;

/// One file or directory as the filesystem knows it, whatever path reached it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId {
    volume: u64,
    file: u128,
}

impl FileId {
    /// Follows symlinks: the identity is the target's.
    #[cfg(unix)]
    pub(crate) fn of(path: &Path) -> std::io::Result<FileId> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path)?;
        Ok(FileId {
            volume: meta.dev(),
            file: u128::from(meta.ino()),
        })
    }

    /// Std has no stable file id on Windows (rust#63010). `FILE_ID_INFO` carries the 128-bit id
    /// ReFS needs, where the older 64-bit index is not unique. Opened with no access and
    /// `FILE_FLAG_BACKUP_SEMANTICS`, the only way to get a handle on a directory.
    #[cfg(windows)]
    pub(crate) fn of(path: &Path) -> std::io::Result<FileId> {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandleEx,
        };
        let file = std::fs::OpenOptions::new()
            .access_mode(0)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        // SAFETY: FILE_ID_INFO is plain integers and bytes, valid all-zero.
        let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a FILE_ID_INFO and the size passed is its own; the handle lives as
        // long as `file`.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                (&raw mut info).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(FileId {
            volume: info.VolumeSerialNumber,
            file: u128::from_le_bytes(info.FileId.Identifier),
        })
    }
}

/// One install folder, less the folder that holds the native plugins' data dirs.
pub(crate) struct Protected {
    root: Place,
    except: Place,
}

/// A folder known by identity, and by its resolved spelling as well: a file system that reports no
/// file id (exFAT, some network redirectors) must not switch the guard off.
struct Place {
    id: Option<FileId>,
    path: Option<std::path::PathBuf>,
}

impl Place {
    fn new(path: &Path) -> Place {
        Place {
            id: FileId::of(path).ok(),
            path: resolved(path),
        }
    }

    /// `id` is `path`'s own, read once by the caller for both places.
    fn is(&self, path: &Path, id: Option<FileId>) -> bool {
        (self.id.is_some() && self.id == id)
            || self.path.as_deref().is_some_and(|own| same_path(path, own))
    }
}

impl Protected {
    /// `except` may not exist yet: the first native plugin to register creates it.
    pub(crate) fn new(install_root: &Path, except: &Path) -> Protected {
        let root = Place::new(install_root);
        if root.id.is_none() {
            crate::verr!(
                "[GUARD]  No file id for {}; protecting it by its canonical path only",
                install_root.display()
            );
        }
        Protected {
            root,
            except: Place::new(except),
        }
    }

    /// Whether writing `path` would touch the install folder. Fails closed on a path it cannot
    /// judge: one with `..` (resolved after symlinks on POSIX, lexically on Windows), or one with no
    /// existing ancestor.
    pub(crate) fn covers(&self, path: &Path) -> bool {
        if path
            .components()
            .any(|part| part == Component::ParentDir || is_windows_alias(part))
        {
            return true;
        }
        let Some(resolved) = resolved(path) else {
            return true;
        };
        // The nearer of the two decides, since the data dirs sit inside the install folder on Linux.
        resolved
            .ancestors()
            .find_map(|ancestor| {
                let id = FileId::of(ancestor).ok();
                if self.except.is(ancestor, id) {
                    Some(false)
                } else if self.root.is(ancestor, id) {
                    Some(true)
                } else {
                    None
                }
            })
            .unwrap_or(false)
    }
}

/// Windows and macOS file systems compare names without case by default.
fn same_name(protected: &str, name: &str) -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        protected.eq_ignore_ascii_case(name)
    } else {
        protected == name
    }
}

/// A Windows spelling that names another entry than it reads: Win32 strips a trailing dot or
/// space, and `name:stream` writes a stream of `name`. Refused outright rather than normalised.
fn is_windows_alias(part: Component) -> bool {
    let Component::Normal(name) = part else {
        return false;
    };
    let name = name.to_string_lossy();
    cfg!(windows) && (name.ends_with(['.', ' ']) || name.contains(':'))
}

/// Component by component, with `same_name`'s case rule.
fn same_path(a: &Path, b: &Path) -> bool {
    let (mut a, mut b) = (a.components(), b.components());
    loop {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(x), Some(y))
                if same_name(
                    &x.as_os_str().to_string_lossy(),
                    &y.as_os_str().to_string_lossy(),
                ) => {}
            _ => return false,
        }
    }
}

/// The deepest existing ancestor canonicalised (symlinks and junctions resolved), the rest
/// rejoined as written.
fn resolved(path: &Path) -> Option<std::path::PathBuf> {
    let mut tail = Vec::new();
    let mut cursor = path;
    loop {
        if let Ok(mut base) = cursor.canonicalize() {
            base.extend(tail.iter().rev());
            return Some(base);
        }
        tail.push(cursor.file_name()?);
        cursor = cursor.parent()?;
    }
}

/// The folder the guard protects for a macOS exe: its `.app` when it runs from one, else its own
/// folder, as on the other systems. dyld reports the exe as launched (through a symlink, with
/// `..`), and CEF resolves it before looking for its bundle; this looks at the same one.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn bundle_root(exe: &Path) -> Option<std::path::PathBuf> {
    let exe = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    app_bundle(&exe)
        .or_else(|| exe.parent())
        .map(Path::to_path_buf)
}

/// `<name>.app/Contents/MacOS/<exe>`, read off the path alone as Chromium and Tauri do.
#[cfg(any(target_os = "macos", test))]
fn app_bundle(exe: &Path) -> Option<&Path> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    let shaped = macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app";
    shaped.then_some(app)
}

static PROTECTED: OnceLock<Protected> = OnceLock::new();

/// After the update recovery, which may roll the protected folder's own files back.
pub(crate) fn install(protected: Protected, _: &crate::updater::Recovered) {
    if PROTECTED.set(protected).is_err() {
        crate::verr!("[GUARD]  The protected folder was set twice; keeping the first one");
    }
}

pub(crate) fn get() -> Option<&'static Protected> {
    PROTECTED.get()
}

#[cfg(test)]
#[path = "../../tests/unit/platform/install_guard.rs"]
mod tests;
