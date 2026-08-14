//! Native file open/save dialog via CEF `BrowserHost::run_file_dialog`. Backs
//! `showOpenDialog` and `showSaveDialog` from `@luna/lib.native`. The dialog
//! must run on the CEF UI thread. The work is posted there as a Task; the
//! selected paths are returned via a oneshot channel (empty = cancelled).

use cef::*;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// Unspent dialog answers. A plugin names its own destination: this is what separates a path
/// the user picked from one the renderer invented. Bounded: dialogs a plugin opens and never uses
/// would grow it without end.
static USER_CHOICES: Mutex<Vec<Choice>> = Mutex::new(Vec::new());
const MAX_USER_CHOICES: usize = 32;

/// Distinguishes one answer from the next for the same path. Matching by path alone let `consume`
/// spend a fresh entry that superseded mid-download, while the write had used the old one.
static NEXT_CHOICE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Directories the user picked in a folder dialog. An album is many files behind one answer; the
/// unit of consent there is the directory and not a name the plugin composed inside it. Session
/// only: process memory, nothing persisted, gone when the app closes.
static GRANTED_FOLDERS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
const MAX_GRANTED_FOLDERS: usize = 32;

struct Choice {
    id: u64,
    resolved: PathBuf,
    replace_confirmed: bool,
}

/// What makes one write legitimate. The two carry different lifetimes on purpose, and the difference
/// lives in the type rather than a flag: `consume` spends a file answer and must not touch a folder
/// grant, and a conditional consume is what spent the wrong entry once already.
pub(crate) enum Authorisation {
    /// One save dialog answer for one exact path, matched by identity and spent once written.
    File { id: u64, replace_confirmed: bool },
    /// A directory the user picked in a folder dialog. Lives for the session, is never spent, and
    /// never replaces anything: no prompt named any file inside it.
    Folder,
}

impl Authorisation {
    /// Did the user answer a prompt about replacing this exact file? False when it did not exist,
    /// when sanitisation renamed it since the prompt then named something else, and always under a
    /// folder grant.
    pub(crate) fn replace_confirmed(&self) -> bool {
        matches!(
            self,
            Self::File {
                replace_confirmed: true,
                ..
            }
        )
    }

    /// A file already on disk under a folder grant is left alone and reported as done, which is what
    /// lets a re-run over a part-downloaded album finish instead of failing track by track.
    pub(crate) fn skips_existing(&self) -> bool {
        matches!(self, Self::Folder)
    }
}

/// The ledger key: the destination as the kernel will resolve it for the write.
///
/// `..` is refused rather than resolved: POSIX resolves it after following symlinks, Windows
/// collapses it lexically first; no single key matches both, and a dialog answer never carries one.
/// The directory is canonicalised as the write will resolve it, but the final component stays literal
/// since `rename` does not follow a final symlink (canonicalising it would make a link and its target
/// share an entry). Ancestors not yet created cannot be canonicalised. The deepest existing one is
/// resolved instead, and the rest rejoined.
fn resolved(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    if path
        .components()
        .any(|part| part == std::path::Component::ParentDir)
    {
        return None;
    }
    let name = path.file_name()?;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cursor = path.parent()?;
    loop {
        if let Ok(base) = cursor.canonicalize() {
            let mut out = base;
            out.extend(tail.iter().rev());
            out.push(name);
            return Some(out);
        }
        tail.push(cursor.file_name()?);
        cursor = cursor.parent()?;
    }
}

/// Record what a dialog answered. Called on the dialog's own result: nothing the renderer
/// says can put a path in here.
pub(crate) fn record_user_choice(path: &Path, replace_confirmed: bool) {
    let Some(resolved) = resolved(path) else {
        crate::verr!(
            "[DIALOG] Cannot resolve the chosen destination: {}",
            path.display()
        );
        return;
    };
    let mut choices = USER_CHOICES.lock().unwrap_or_else(|e| e.into_inner());
    record_bounded(
        &mut choices,
        Choice {
            id: NEXT_CHOICE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            resolved,
            replace_confirmed,
        },
    );
}

/// Replaces any entry for the same destination, then bounds (in that order). One entry per
/// destination means a fresh answer needs no slot; bounding first destroyed an unrelated
/// authorisation and refused a download whose user had answered its dialog. Kept off the global,
/// testable without evicting entries a concurrent test holds.
fn record_bounded(choices: &mut Vec<Choice>, choice: Choice) {
    // One entry per destination: a fresh answer supersedes an unspent one. Appending left the oldest
    // in front of `find` for good. A retry kept a stale `replace_confirmed`, and `consume` could
    // not tell duplicates apart.
    choices.retain(|c| c.resolved != choice.resolved);
    if choices.len() >= MAX_USER_CHOICES {
        // Announced, not silent: the download that had this answer will be refused with a
        // message about no dialog, which would otherwise contradict what the user just did.
        let dropped = choices.remove(0);
        crate::verr!(
            "[DIALOG] {MAX_USER_CHOICES} dialog answers unused, dropping the oldest: {}",
            dropped.resolved.display()
        );
    }
    choices.push(choice);
}

/// The authorisation for `dest`: a save dialog answer for that exact path, else a folder grant
/// containing it. Does not consume: an attempt that fails downstream must not cost the user their
/// answer.
///
/// The file answer is checked first, being the more specific consent and the only one that can carry
/// a replace decision.
pub(crate) fn authorisation_for(dest: &Path) -> Option<Authorisation> {
    let wanted = resolved(dest)?;
    let choices = USER_CHOICES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(choice) = choices.iter().find(|c| c.resolved == wanted) {
        return Some(Authorisation::File {
            id: choice.id,
            replace_confirmed: choice.replace_confirmed,
        });
    }
    drop(choices);
    folder_grant_covers(&wanted).then_some(Authorisation::Folder)
}

/// Spend an authorisation. Called once the file is on disk; one save dialog answer buys one
/// written file. Matched by id: an answer that arrived for the same path while the write ran is a
/// different entry, and spending it would consume consent nothing used.
///
/// A folder grant is not spendable: an album is many writes behind one answer.
pub(crate) fn consume(auth: Authorisation) {
    let Authorisation::File { id, .. } = auth else {
        return;
    };
    let mut choices = USER_CHOICES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(index) = choices.iter().position(|c| c.id == id) {
        choices.remove(index);
    }
}

/// Record a directory a folder dialog answered. Called on the dialog's own result, like
/// `record_user_choice`. Nothing the renderer says can put a path in here.
///
/// Canonicalised outright rather than through `resolved`: the user just picked it, and it exists.
/// The key is the directory itself rather than a directory plus a final component.
pub(crate) fn record_folder_grant(dir: &Path) {
    let Ok(canonical) = dir.canonicalize() else {
        crate::verr!(
            "[DIALOG] Cannot resolve the chosen folder, downloads to it will be refused: {}",
            dir.display()
        );
        return;
    };
    let mut granted = GRANTED_FOLDERS.lock().unwrap_or_else(|e| e.into_inner());
    push_bounded(&mut granted, canonical);
}

/// Deduplicate and bound. Kept off the global; the rule can be tested without evicting grants a
/// concurrent test holds, since libtest runs tests as threads in one process.
fn push_bounded(granted: &mut Vec<PathBuf>, canonical: PathBuf) {
    // Re-picking the same folder is the same consent, not a second one: appending would let a user
    // evict their own earlier grants by answering one dialog repeatedly.
    if granted.contains(&canonical) {
        return;
    }
    if granted.len() >= MAX_GRANTED_FOLDERS {
        // Announced, not silent: a download into the dropped folder is refused with a message about
        // no dialog, which would otherwise contradict what the user remembers doing.
        let dropped = granted.remove(0);
        crate::verr!(
            "[DIALOG] {MAX_GRANTED_FOLDERS} folder grants held, dropping the oldest: {}",
            dropped.display()
        );
    }
    granted.push(canonical);
}

/// Does a granted folder contain this destination? `dest_resolved` must already have been through
/// `resolved`, which refuses `..` and canonicalises the directory; this is a component-wise
/// containment test between two canonical paths rather than a string prefix.
fn folder_grant_covers(dest_resolved: &Path) -> bool {
    GRANTED_FOLDERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|granted| dest_resolved.starts_with(granted))
}

/// Show a native file dialog. Returns a receiver resolving to the selected
/// paths (empty Vec = cancelled). Safe to call from any thread.
pub(crate) fn show_file_dialog(
    mode: FileDialogMode,
    title: Option<String>,
    default_path: Option<String>,
    filters: Vec<String>,
) -> oneshot::Receiver<Vec<String>> {
    let (tx, rx) = oneshot::channel();
    let sender = Arc::new(Mutex::new(Some(tx)));
    let mut task = FileDialogTask::new(mode, title, default_path, filters, sender);
    post_task(ThreadId::UI, Some(&mut task));
    rx
}

// --- Callback: receives the dismissal on the UI thread ---

/// `IntoIterator` consumes the list and cloning is no way around it: a clone of the borrowed
/// list CEF hands us becomes a variant `into_iter` reads as empty. The raw pointer round trip
/// keeps it readable, and frees nothing, since only an owned list is freed.
fn read_paths(list: &mut CefStringList) -> Vec<String> {
    let raw: *mut sys::_cef_string_list_t = list.into();
    CefStringList::from(raw).into_iter().collect()
}

wrap_run_file_dialog_callback! {
    struct FileDialogCallback {
        sender: Arc<Mutex<Option<oneshot::Sender<Vec<String>>>>>,
    }
    impl RunFileDialogCallback {
        fn on_file_dialog_dismissed(&self, file_paths: Option<&mut CefStringList>) {
            let paths: Vec<String> = file_paths.map(read_paths).unwrap_or_default();
            if let Some(tx) = self.sender.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = tx.send(paths);
            }
        }
    }
}

// --- Task: runs on the CEF UI thread, invokes run_file_dialog ---

wrap_task! {
    struct FileDialogTask {
        mode: FileDialogMode,
        title: Option<String>,
        default_path: Option<String>,
        filters: Vec<String>,
        sender: Arc<Mutex<Option<oneshot::Sender<Vec<String>>>>>,
    }
    impl Task {
        fn execute(&self) {
            let Some(host) = crate::app_state::with_state(|s| s.browser.clone())
                .flatten()
                .and_then(|b| b.host())
            else {
                // No browser/host: resolve empty; the JS promise must still settle.
                if let Some(tx) = self.sender.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = tx.send(Vec::new());
                }
                return;
            };

            let title = self.title.as_deref().map(CefString::from);
            let default_path = self.default_path.as_deref().map(CefString::from);

            let mut filters = CefStringList::new();
            for f in &self.filters {
                filters.append(f);
            }
            let mut cb = FileDialogCallback::new(self.sender.clone());

            // Always a list, never NULL: CEF documents only `title` as nullable and
            // dereferences `accept_filters` to transfer its contents. Empty = no restriction.
            host.run_file_dialog(
                self.mode,
                title.as_ref(),
                default_path.as_ref(),
                Some(&mut filters),
                Some(&mut cb),
            );
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ui/file_dialog.rs"]
mod tests;
