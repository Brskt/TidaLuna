//! macOS: `NSOpenPanel`, started asynchronously: `runModal` would nest a native loop that holds
//! back CEF's tasks and its quit for as long as the panel is open.

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::Path;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSModalPanelWindowLevel, NSModalResponse, NSModalResponseOK, NSNormalWindowLevel, NSOpenPanel,
    NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSString, NSURL};

use super::Answer;
use crate::util::one_at_a_time::OneAtATime;

thread_local! {
    /// The panel on screen and the guard it holds. Main thread only.
    static OPEN: RefCell<Option<(Retained<NSOpenPanel>, OneAtATime)>> = const { RefCell::new(None) };
}

pub(super) fn open(
    guard: OneAtATime,
    owner: usize,
    title: Option<String>,
    default_path: Option<String>,
    slot: Answer,
) {
    let Some(mtm) = MainThreadMarker::new() else {
        crate::verr!("[DIALOG] Folder picker asked off the main thread");
        slot.send(Vec::new());
        return;
    };
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(false);
    panel.setCanChooseDirectories(true);
    panel.setAllowsMultipleSelection(false);
    panel.setCanCreateDirectories(true);
    if let Some(title) = title.filter(|title| !title.is_empty()) {
        panel.setMessage(Some(&NSString::from_str(&title)));
    }
    if let Some(start) = default_path {
        let url = NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&start), true);
        panel.setDirectoryURL(Some(&url));
    }

    OPEN.with(|open| *open.borrow_mut() = Some((panel.clone(), guard)));
    let in_handler = panel.clone();
    let handler = RcBlock::new(move |response: NSModalResponse| {
        // The sheet can still be on screen here, and a refusal or a new panel may follow at once.
        in_handler.orderOut(None);
        OPEN.with(|open| open.borrow_mut().take());
        let paths = if response == NSModalResponseOK {
            in_handler
                .URL()
                .filter(|url| url.isFileURL())
                .and_then(|url| url.path())
                .map(|path| path.to_string())
                .into_iter()
                // Before macOS 14 a file can come back despite `canChooseFiles(false)`.
                .filter(|path| Path::new(path).is_dir())
                .collect()
        } else {
            Vec::new()
        };
        slot.send(paths);
    });

    match sheet_parent(owner as *mut c_void) {
        Some(window) => panel.beginSheetModalForWindow_completionHandler(&window, &handler),
        None => {
            panel.setLevel(NSModalPanelWindowLevel);
            panel.beginWithCompletionHandler(&handler);
        }
    }
}

/// The window a sheet can hang from (titled, normal level), and only while it is
/// on screen: a sheet on a window hidden to the tray would be invisible.
fn sheet_parent(handle: *mut c_void) -> Option<Retained<NSWindow>> {
    // SAFETY: on macOS CEF hands back the widget's NSView, valid on the UI thread; null gives None.
    let view = unsafe { Retained::retain(handle.cast::<NSView>()) }?;
    let window = view.window()?;
    let hosts = window.styleMask().contains(NSWindowStyleMask::Titled)
        && window.level() == NSNormalWindowLevel
        && window.isVisible()
        && !window.isMiniaturized();
    hosts.then_some(window)
}

/// The main window is going away: the open panel answers as cancelled.
pub(crate) fn cancel_open() {
    if let Some(panel) = OPEN.with(|open| open.borrow().as_ref().map(|(panel, _)| panel.clone())) {
        // SAFETY: `cancel:` takes an optional sender.
        unsafe { panel.cancel(None) };
    }
}
