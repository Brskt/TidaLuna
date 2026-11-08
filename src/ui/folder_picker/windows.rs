//! Windows: `IFileOpenDialog` on a COM thread of its own. `Show` disables its owner as well, but
//! only from the dialog's thread once it runs: the main window is disabled here first and enabled
//! again on the UI thread.

use std::sync::{Arc, Mutex};

use cef::rc::Rc as _;
use cef::{ImplTask, Task, ThreadId, WrapTask, post_task, wrap_task};
use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize, IBindCtx,
};
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem,
    SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::core::{HRESULT, HSTRING, IUnknown};

use super::Answer;
use crate::util::one_at_a_time::OneAtATime;

enum Outcome {
    Chosen(String),
    Cancelled,
    Failed(String),
}

pub(super) fn open(
    guard: OneAtATime,
    owner: usize,
    title: Option<String>,
    default_path: Option<String>,
    slot: Answer,
) {
    set_enabled(owner, false);
    let in_thread = slot.clone();
    let spawned = std::thread::Builder::new()
        .name("folder-picker".into())
        .spawn(move || {
            match run(owner, title.as_deref(), default_path.as_deref()) {
                Outcome::Chosen(path) => in_thread.send(vec![path]),
                Outcome::Cancelled => in_thread.send(Vec::new()),
                Outcome::Failed(why) => {
                    crate::verr!("[DIALOG] Folder picker failed: {why}");
                    in_thread.send(Vec::new());
                }
            }
            // The guard goes with the owner: a new picker never disables a window this one is
            // about to enable.
            let mut task = EnableOwner::new(owner, Arc::new(Mutex::new(Some(guard))));
            post_task(ThreadId::UI, Some(&mut task));
        });
    if let Err(e) = spawned {
        crate::verr!("[DIALOG] Could not start the folder picker thread: {e}");
        set_enabled(owner, true);
        slot.send(Vec::new());
    }
}

/// UI thread only, and only on a window that still exists.
fn set_enabled(owner: usize, enabled: bool) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindow;
    let hwnd = owner as windows_sys::Win32::Foundation::HWND;
    // SAFETY: IsWindow accepts any value, and EnableWindow is only reached for a live window.
    unsafe {
        if owner != 0 && IsWindow(hwnd) != 0 {
            EnableWindow(hwnd, i32::from(enabled));
        }
    }
}

wrap_task! {
    struct EnableOwner {
        owner: usize,
        guard: Arc<Mutex<Option<OneAtATime>>>,
    }
    impl Task {
        fn execute(&self) {
            set_enabled(self.owner, true);
            self.guard.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
    }
}

/// Undoes `CoInitializeEx` on the thread that called it.
struct ComScope;

impl Drop for ComScope {
    fn drop(&mut self) {
        // SAFETY: only built after a successful CoInitializeEx on this same thread.
        unsafe { CoUninitialize() };
    }
}

fn run(owner: usize, title: Option<&str>, default_path: Option<&str>) -> Outcome {
    // SAFETY: a fresh thread, initialised once and undone by `ComScope` on the same thread.
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    if hr.is_err() {
        return Outcome::Failed(format!("CoInitializeEx {hr:?}"));
    }
    let _com = ComScope;
    match pick(owner, title, default_path) {
        Ok(outcome) => outcome,
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

pub(super) fn is_cancel(code: HRESULT) -> bool {
    code == HRESULT::from_win32(ERROR_CANCELLED.0)
}

/// A path the dialog returned. `None` for a name with an unpaired surrogate, which NTFS allows: a
/// lossy conversion would name another folder.
pub(super) fn wide_to_path(wide: &[u16]) -> Option<String> {
    String::from_utf16(wide)
        .ok()
        .filter(|path| !path.is_empty())
}

fn pick(
    owner: usize,
    title: Option<&str>,
    default_path: Option<&str>,
) -> windows::core::Result<Outcome> {
    // SAFETY: COM is initialised on this thread (see `run`); every pointer comes from COM itself,
    // and the display name is copied before it is freed.
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None::<&IUnknown>, CLSCTX_INPROC_SERVER)?;
        let defaults = dialog.GetOptions()?;
        // FORCEFILESYSTEM keeps virtual folders out, which have no file system path.
        dialog.SetOptions(defaults | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)?;
        if let Some(title) = title.filter(|title| !title.is_empty()) {
            dialog.SetTitle(&HSTRING::from(title))?;
        }
        // An unusable start folder is skipped, never failing the dialog.
        if let Some(start) = default_path
            && let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(
                &HSTRING::from(start),
                None::<&IBindCtx>,
            )
        {
            let _ = dialog.SetFolder(&item);
        }
        if let Err(e) = dialog.Show(Some(HWND(owner as *mut _))) {
            return if is_cancel(e.code()) {
                Ok(Outcome::Cancelled)
            } else {
                Err(e)
            };
        }
        let name = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
        let wide = name.as_wide().to_vec();
        CoTaskMemFree(Some(name.as_ptr().cast_const().cast()));
        Ok(match wide_to_path(&wide) {
            Some(path) => Outcome::Chosen(path),
            None => Outcome::Failed("the chosen folder's name is not valid Unicode".into()),
        })
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/ui/folder_picker_windows.rs"]
mod tests;
