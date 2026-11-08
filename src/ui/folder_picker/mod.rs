//! The folder picker, opened through the system rather than CEF. CEF's folder mode is Chrome's
//! upload-folder path, which lists the whole chosen folder before it answers: seconds on a home
//! folder, minutes on a large library.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::util::one_at_a_time::OneAtATime;
use crate::util::reply::OnceReply;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as native;
#[cfg(target_os = "macos")]
use macos as native;
#[cfg(target_os = "windows")]
use windows as native;

#[cfg(target_os = "macos")]
pub(crate) use macos::cancel_open;

/// The dialog's single answer, the same slot CEF's callback fills: empty means cancelled.
pub(crate) type Answer = Arc<OnceReply<Vec<String>>>;

/// Set while a picker is on screen; a second one is refused.
static OPEN: AtomicBool = AtomicBool::new(false);

/// The folder to open on: `default_path` when it is a directory, else its parent. Reads the disk.
pub(crate) fn start_folder(default_path: Option<&str>) -> Option<PathBuf> {
    let raw = default_path.filter(|p| !p.is_empty() && !p.contains('\0'))?;
    let path = Path::new(raw);
    if path.is_dir() {
        return Some(path.to_path_buf());
    }
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

/// Open the system folder picker on `default_path`, a `start_folder` already read off the UI thread,
/// and answer `slot`. Runs on the UI thread, where `owner` (the main window's native handle, 0 when
/// there is none) was read.
pub(crate) fn open(
    owner: usize,
    title: Option<String>,
    default_path: Option<String>,
    slot: Answer,
) {
    let Some(guard) = OneAtATime::try_acquire(&OPEN) else {
        crate::verr!("[DIALOG] A folder picker is already open, refusing a second");
        slot.send(Vec::new());
        return;
    };
    native::open(guard, owner, title, default_path, slot);
}

#[cfg(test)]
#[path = "../../../tests/unit/ui/folder_picker.rs"]
mod tests;
