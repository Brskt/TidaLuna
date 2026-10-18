//! Directory-entry durability for files whose loss, or return from the dead, reverses a security
//! decision: the credential file a logout erases, the high-water mark a replayed manifest is
//! refused against.
//!
//! `File::sync_all` makes a file's CONTENT durable and says nothing about its NAME. The entry
//! that a rename created or an unlink removed lives in the parent directory's metadata, which
//! the filesystem commits on its own schedule (ext4: every 5 s by default; a power cut loses
//! up to that much). An erased file can come back, a replaced one can revert.
//!
//! Linux and macOS: fsync the parent directory, as `fsync(2)` prescribes. macOS erases no file
//! here, its credentials living in the Keychain: the erase exists for Linux and Windows. Windows
//! documents no way to flush a directory (`FlushFileBuffers` lists files, volumes and pipes; the
//! directory handle page does not list it). An erase there is made durable through the file
//! instead (see [`remove_file`]) and a replace through the one documented knob,
//! `MOVEFILE_WRITE_THROUGH`.

#[cfg(any(target_os = "linux", target_os = "windows", test))]
use std::fs::OpenOptions;
use std::io;
use std::path::Path;

/// Flushes the directory that holds `path`, committing the creation, rename or removal of its
/// entry. Does nothing on Windows, which has no documented call for it.
#[cfg(unix)]
pub(crate) fn sync_parent(path: &Path) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(windows)]
pub(crate) fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Renames the synced temporary file onto `dest` and makes the new name durable. The caller has
/// already called `sync_all` on the file: this adds the entry, never the content.
#[cfg(unix)]
pub(crate) fn persist(tmp: tempfile::NamedTempFile, dest: &Path) -> io::Result<()> {
    tmp.persist(dest).map_err(|e| e.error)?;
    sync_parent(dest)
}

/// `tempfile`'s own rename asks Windows for `MOVEFILE_REPLACE_EXISTING` alone, which returns
/// before the move is on the disk; this one adds `MOVEFILE_WRITE_THROUGH`, "the function does not
/// return until the file is actually moved on the disk".
#[cfg(windows)]
pub(crate) fn persist(tmp: tempfile::NamedTempFile, dest: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    // Closes the handle (the move wants no open writer) and keeps the delete-on-drop: a failed
    // move leaves no temporary file, a successful one leaves nothing for the drop to find.
    let staged = tmp.into_temp_path();
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    let moved = unsafe {
        MoveFileExW(
            wide(&staged).as_ptr(),
            wide(dest).as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Empties the file at `path` and syncs it: afterwards the file reads as absent to
/// `secure_store::load` whether or not its directory entry survives. Absent is already done.
///
/// This is the erase's durable part where the directory cannot be flushed. The truncation is a
/// change of the file's own size, which `sync_all` does cover on every platform.
#[cfg(any(target_os = "linux", target_os = "windows", test))]
pub(crate) fn tombstone(path: &Path) -> io::Result<()> {
    match OpenOptions::new().write(true).truncate(true).open(path) {
        Ok(file) => file.sync_all(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Removes `path` and keeps it removed across a power cut. Already absent is success.
///
/// Three steps, each run even when the one before failed: the tombstone first (the erase then
/// survives even where the directory cannot be flushed), then the unlink, then the directory
/// flush. The first error is the one returned.
#[cfg(any(target_os = "linux", target_os = "windows", test))]
pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
    let emptied = tombstone(path);
    let unlinked = ignoring_not_found(std::fs::remove_file(path));
    // A missing directory holds no entry to flush.
    let flushed = ignoring_not_found(sync_parent(path));
    emptied.and(unlinked).and(flushed)
}

#[cfg(any(target_os = "linux", target_os = "windows", test))]
fn ignoring_not_found(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/platform/durable_fs.rs"]
pub(crate) mod tests;
