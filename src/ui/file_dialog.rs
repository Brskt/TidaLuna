//! Native file open/save dialog via CEF `BrowserHost::run_file_dialog`. Backs
//! `showOpenDialog` and `showSaveDialog` from `@luna/lib.native`. The dialog
//! must run on the CEF UI thread, so the work is posted there as a Task; the
//! selected paths are returned via a oneshot channel (empty = cancelled).

use cef::*;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

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

wrap_run_file_dialog_callback! {
    struct FileDialogCallback {
        sender: Arc<Mutex<Option<oneshot::Sender<Vec<String>>>>>,
    }
    impl RunFileDialogCallback {
        fn on_file_dialog_dismissed(&self, file_paths: Option<&mut CefStringList>) {
            // CefStringList only exposes a consuming IntoIterator; clone the borrow.
            let paths: Vec<String> = file_paths
                .map(|list| list.clone().into_iter().collect())
                .unwrap_or_default();
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
                // No browser/host: resolve empty so the JS promise still settles.
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
            let filters_arg = if self.filters.is_empty() {
                None
            } else {
                Some(&mut filters)
            };

            let mut cb = FileDialogCallback::new(self.sender.clone());

            host.run_file_dialog(
                self.mode,
                title.as_ref(),
                default_path.as_ref(),
                filters_arg,
                Some(&mut cb),
            );
        }
    }
}
