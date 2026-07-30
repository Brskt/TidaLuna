//! Shared native CEF Views dialog. Each dialog (trust / crash / message) is a
//! separate top-level window isolated from the main renderer: its buttons
//! navigate to a custom-scheme URL (`trust://`, `crash://`, `msgbox://`) which
//! the dialog's own RequestHandler intercepts and maps to a result. This module
//! owns the shared skeleton (client, delegates, the UI-thread task, centering,
//! the ALLOY runtime style, and the fire-the-oneshot-exactly-once logic); each
//! caller supplies only its HTML, size, a `url -> Option<R>` parser, and the
//! result to send if the window is closed without a button.

use cef::*;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

pub(crate) fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Result-specific state shared by the dialog's request handler, window
/// delegate, and UI-thread task. A single `Arc<DialogCore<R>>` reaches both the
/// request handler (button click) and the window delegate (close-without-click),
/// so the oneshot sender behind the mutex is `take()`n exactly once.
struct DialogCore<R> {
    sender: Mutex<Option<oneshot::Sender<R>>>,
    /// Maps a button-navigation URL to its result, or `None` for any other nav.
    parse: fn(&str) -> Option<R>,
    /// Result sent when the window is closed without a button click.
    on_close: R,
    width: i32,
    height: i32,
}

impl<R> DialogCore<R> {
    fn send(&self, r: R) {
        if let Some(tx) = self.sender.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = tx.send(r);
        }
    }
}

/// Show a dialog rendering `html` and return the user's choice via a oneshot.
/// Can be called from any thread - internally posts to the CEF UI thread.
pub(crate) fn show_dialog<R: Clone + Send + Sync + 'static>(
    html: String,
    size: (i32, i32),
    parse: fn(&str) -> Option<R>,
    on_close: R,
) -> oneshot::Receiver<R> {
    let (tx, rx) = oneshot::channel();
    let core = Arc::new(DialogCore {
        sender: Mutex::new(Some(tx)),
        parse,
        on_close,
        width: size.0,
        height: size.1,
    });
    let mut task = ShowDialogTask::new(html, core);
    post_task(ThreadId::UI, Some(&mut task));
    rx
}

/// DPI scale factor for a window (physical pixels / DIP). Returns 1.0 on failure.
#[cfg(target_os = "windows")]
unsafe fn dip_scale(hwnd: windows_sys::Win32::Foundation::HWND) -> f64 {
    use windows_sys::Win32::Graphics::Gdi::{GetDC, GetDeviceCaps, LOGPIXELSX, ReleaseDC};
    // SAFETY: hwnd is a valid window handle obtained from CEF's window_handle()
    let dc = unsafe { GetDC(hwnd) };
    let dpi = if !dc.is_null() {
        let d = unsafe { GetDeviceCaps(dc, LOGPIXELSX as i32) };
        unsafe { ReleaseDC(hwnd, dc) };
        d
    } else {
        96
    };
    if dpi > 0 { dpi as f64 / 96.0 } else { 1.0 }
}

/// Returns the main app window's HWND, or null.
#[cfg(target_os = "windows")]
fn get_main_hwnd() -> windows_sys::Win32::Foundation::HWND {
    crate::app_state::with_state(|s| s.browser.clone())
        .flatten()
        .and_then(|b| b.host())
        .map(|h| h.window_handle().0 as windows_sys::Win32::Foundation::HWND)
        .unwrap_or(std::ptr::null_mut())
}

/// Center the dialog on the main app window, clamped to the monitor work area.
/// Returns false if positioning failed (caller should fall back to center_window).
#[cfg(target_os = "windows")]
fn center_on_parent(window: &mut Window, dialog_w: i32, dialog_h: i32) -> bool {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;

    let hwnd = get_main_hwnd();
    if hwnd.is_null() {
        return false;
    }

    // Get parent window rect in DIP
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return false;
    }
    let scale = unsafe { dip_scale(hwnd) };
    let parent_x = (rect.left as f64 / scale) as i32;
    let parent_y = (rect.top as f64 / scale) as i32;
    let parent_w = ((rect.right - rect.left) as f64 / scale) as i32;
    let parent_h = ((rect.bottom - rect.top) as f64 / scale) as i32;

    // Get monitor work area in DIP
    let hmonitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if hmonitor.is_null() {
        return false;
    }
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        rcMonitor: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        rcWork: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        dwFlags: 0,
    };
    if unsafe { GetMonitorInfoW(hmonitor, &mut mi) } == 0 {
        return false;
    }
    let work_left = (mi.rcWork.left as f64 / scale) as i32;
    let work_top = (mi.rcWork.top as f64 / scale) as i32;
    let work_right = (mi.rcWork.right as f64 / scale) as i32;
    let work_bottom = (mi.rcWork.bottom as f64 / scale) as i32;

    // Center on parent, clamp within work area
    let x = (parent_x + (parent_w - dialog_w) / 2)
        .clamp(work_left, (work_right - dialog_w).max(work_left));
    let y = (parent_y + (parent_h - dialog_h) / 2)
        .clamp(work_top, (work_bottom - dialog_h).max(work_top));

    window.set_bounds(Some(&cef::Rect {
        x,
        y,
        width: dialog_w,
        height: dialog_h,
    }));
    true
}

#[cfg(not(target_os = "windows"))]
fn center_on_parent(_window: &mut Window, _dialog_w: i32, _dialog_h: i32) -> bool {
    false
}

// --- Request handler: intercepts the dialog's custom-scheme button navigations ---

wrap_request_handler! {
    struct DialogRequestHandler<R: Clone + Send + Sync + 'static> {
        core: Arc<DialogCore<R>>,
    }
    impl RequestHandler {
        fn on_before_browse(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _user_gesture: ::std::os::raw::c_int,
            _is_redirect: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let url = request
                .as_ref()
                .map(|r| {
                    let u = r.url();
                    crate::ui::token_filter::userfree_to_string(&u)
                })
                .unwrap_or_default();

            if let Some(result) = (self.core.parse)(&url) {
                self.core.send(result);
                if let Some(b) = browser
                    && let Some(host) = b.host()
                {
                    host.try_close_browser();
                }
                return 1;
            }
            0
        }
        fn resource_request_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int,
            _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            None
        }
        // Isolation: a crash in the dialog's own renderer must not re-enter the
        // main crash handler and stack dialogs.
        fn on_render_process_terminated(
            &self,
            _browser: Option<&mut Browser>,
            _status: TerminationStatus,
            _error_code: ::std::os::raw::c_int,
            _error_string: Option<&CefString>,
        ) {
        }
    }
}

// --- Minimal life span handler ---

wrap_life_span_handler! {
    struct DialogLifeSpanHandler {
        _p: u8,
    }
    impl LifeSpanHandler {}
}

// --- Client ---

wrap_client! {
    struct DialogClient {
        life_span: LifeSpanHandler,
        request: RequestHandler,
    }
    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span.clone())
        }
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.request.clone())
        }
    }
}

// --- Window delegate ---

wrap_window_delegate! {
    struct DialogWindowDelegate<R: Clone + Send + Sync + 'static> {
        browser_view: RefCell<Option<BrowserView>>,
        core: Arc<DialogCore<R>>,
    }
    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            Size {
                width: self.core.width,
                height: self.core.height,
            }
        }
    }
    impl PanelDelegate {}
    impl WindowDelegate {
        fn window_runtime_style(&self) -> RuntimeStyle {
            RuntimeStyle::ALLOY
        }
        fn is_frameless(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            1
        }
        fn can_resize(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            0
        }
        fn can_maximize(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            0
        }
        fn can_minimize(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            0
        }
        fn on_window_created(&self, window: Option<&mut Window>) {
            let bv = self.browser_view.borrow();
            let (Some(window), Some(bv)) = (window, bv.as_ref()) else {
                return;
            };
            let mut view = View::from(bv);
            window.add_child_view(Some(&mut view));

            let (dialog_w, dialog_h) = (self.core.width, self.core.height);

            // Center on the main app window, clamped to the monitor work area.
            let placed = center_on_parent(window, dialog_w, dialog_h);
            if !placed {
                window.center_window(Some(&Size {
                    width: dialog_w,
                    height: dialog_h,
                }));
            }

            if let Some(mut icon) = image_create() {
                let png_data = include_bytes!("../../tidaluna.png");
                icon.add_png(1.0, Some(png_data));
                window.set_window_icon(Some(&mut icon));
            }

            window.show();
            window.activate();
        }
        fn on_window_destroyed(&self, _window: Option<&mut Window>) {
            *self.browser_view.borrow_mut() = None;
            // Closing via X without clicking a button → the caller's default.
            self.core.send(self.core.on_close.clone());
        }
        fn can_close(&self, _window: Option<&mut Window>) -> i32 {
            1
        }
    }
}

wrap_browser_view_delegate! {
    struct DialogBrowserViewDelegate {
        _p: u8,
    }
    impl ViewDelegate {}
    impl BrowserViewDelegate {
        fn browser_runtime_style(&self) -> RuntimeStyle {
            RuntimeStyle::ALLOY
        }
    }
}

// --- Task to create the dialog on the CEF UI thread ---

wrap_task! {
    struct ShowDialogTask<R: Clone + Send + Sync + 'static> {
        html: String,
        core: Arc<DialogCore<R>>,
    }
    impl Task {
        fn execute(&self) {
            let life_span = DialogLifeSpanHandler::new(0);
            let request = DialogRequestHandler::new(self.core.clone());
            let mut client = DialogClient::new(life_span, request);

            let settings = BrowserSettings {
                background_color: 0xFF1A1A1A,
                ..Default::default()
            };

            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(self.html.as_bytes());
            let data_url = format!("data:text/html;base64,{b64}");
            let url = CefString::from(data_url.as_str());

            let mut bv_delegate = DialogBrowserViewDelegate::new(0);
            let browser_view = browser_view_create(
                Some(&mut client),
                Some(&url),
                Some(&settings),
                None,
                None,
                Some(&mut bv_delegate),
            );

            let mut window_delegate =
                DialogWindowDelegate::new(RefCell::new(browser_view), self.core.clone());
            window_create_top_level(Some(&mut window_delegate));
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ui/dialog.rs"]
mod tests;
