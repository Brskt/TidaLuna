//! Cross-platform render-crash recovery dialog.
//!
//! Shown when the main page's render process dies. It is a separate CEF window
//! (its own browser, isolated from the dead renderer), built like the native
//! trust dialog. The buttons communicate back via crash:// URL navigation
//! intercepted by this dialog's own RequestHandler.

use super::trust_dialog::escape_html;
use cef::*;
use std::cell::RefCell;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

const CRASH_RELOAD: &str = "crash://reload";
const CRASH_OPEN: &str = "crash://open";
const CRASH_QUIT: &str = "crash://quit";
const DIALOG_W: i32 = 520;
const DIALOG_H: i32 = 300;

/// Set while a crash dialog is on screen so a fresh crash (e.g. the reloaded
/// page dies again) does not stack a second dialog. Reset once the dialog
/// resolves.
pub(crate) static CRASH_DIALOG_OPEN: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrashAction {
    Reload,
    OpenFolder,
    Quit,
}

/// Show the crash dialog and return the user's choice via a oneshot channel.
/// Posts to the CEF UI thread internally; the receiver must be awaited off the
/// UI thread.
pub(crate) fn show_crash_dialog(
    reason: &str,
    error_code: i32,
    log_path: Option<&std::path::Path>,
) -> oneshot::Receiver<CrashAction> {
    let (tx, rx) = oneshot::channel();
    let html = build_html(reason, error_code, log_path);
    let sender = Arc::new(Mutex::new(Some(tx)));
    let mut task = ShowCrashDialogTask::new(html, sender);
    post_task(ThreadId::UI, Some(&mut task));
    rx
}

fn build_html(reason: &str, error_code: i32, log_path: Option<&std::path::Path>) -> String {
    let log_line = match log_path {
        Some(p) => format!(
            r#"<div class="log">Crash log saved to: {}</div>"#,
            escape_html(&p.display().to_string())
        ),
        None => String::new(),
    };

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<style>
* {{ margin:0; padding:0; box-sizing:border-box; }}
body {{
    background:#1a1a1a; color:#fff; font-family:system-ui,sans-serif;
    display:flex; align-items:center; justify-content:center;
    height:100vh; -webkit-app-region:drag;
}}
.dialog {{ max-width:460px; width:90%; -webkit-app-region:no-drag; }}
h2 {{ font-size:16px; margin-bottom:14px; color:#eb1e32; }}
.info {{
    background:#222; padding:10px 14px; border-radius:4px;
    font-size:13px; color:#ccc; line-height:1.6; margin-bottom:16px;
}}
.log {{ font-size:11px; color:#777; word-break:break-all; margin-top:6px; }}
.actions {{ display:flex; gap:8px; justify-content:flex-end; }}
button {{
    padding:8px 16px; border:none; border-radius:4px;
    color:#fff; cursor:pointer; font-size:13px;
}}
.quit {{ background:#333; }}
.quit:hover {{ background:#444; }}
.open {{ background:#333; }}
.open:hover {{ background:#444; }}
.reload {{ background:#eb1e32; }}
.reload:hover {{ background:#d11a2d; }}
</style>
</head>
<body>
<div class="dialog">
    <h2>TidaLunar page crashed</h2>
    <div class="info">
        The page process {reason} (error code {code}).
        {log_line}
    </div>
    <div class="actions">
        <button class="quit" onclick="location.href='{quit}'">Quit</button>
        <button class="open" onclick="location.href='{open}'">Open crash folder</button>
        <button class="reload" onclick="location.href='{reload}'">Reload</button>
    </div>
</div>
</body>
</html>"#,
        reason = escape_html(reason),
        code = error_code,
        log_line = log_line,
        quit = CRASH_QUIT,
        open = CRASH_OPEN,
        reload = CRASH_RELOAD,
    )
}

// --- Request handler: intercepts crash:// navigations ---

wrap_request_handler! {
    struct CrashRequestHandler {
        sender: Arc<Mutex<Option<oneshot::Sender<CrashAction>>>>,
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

            let action = if url.starts_with(CRASH_RELOAD) {
                Some(CrashAction::Reload)
            } else if url.starts_with(CRASH_OPEN) {
                Some(CrashAction::OpenFolder)
            } else if url.starts_with(CRASH_QUIT) {
                Some(CrashAction::Quit)
            } else {
                None
            };

            if let Some(action) = action {
                if let Some(tx) = self.sender.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = tx.send(action);
                }
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
    struct CrashLifeSpanHandler {
        _p: u8,
    }
    impl LifeSpanHandler {}
}

// --- Client ---

wrap_client! {
    struct CrashClient {
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
    struct CrashWindowDelegate {
        browser_view: RefCell<Option<BrowserView>>,
        sender: Arc<Mutex<Option<oneshot::Sender<CrashAction>>>>,
    }
    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            Size {
                width: DIALOG_W,
                height: DIALOG_H,
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

            let placed = super::trust_dialog::center_on_parent(window, DIALOG_W, DIALOG_H);
            if !placed {
                window.center_window(Some(&Size {
                    width: DIALOG_W,
                    height: DIALOG_H,
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
            // Closing via X without a button → reload (safe default).
            if let Some(tx) = self.sender.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = tx.send(CrashAction::Reload);
            }
        }
        fn can_close(&self, _window: Option<&mut Window>) -> i32 {
            1
        }
    }
}

wrap_browser_view_delegate! {
    struct CrashBrowserViewDelegate {
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
    struct ShowCrashDialogTask {
        html: String,
        sender: Arc<Mutex<Option<oneshot::Sender<CrashAction>>>>,
    }
    impl Task {
        fn execute(&self) {
            let life_span = CrashLifeSpanHandler::new(0);
            let request = CrashRequestHandler::new(self.sender.clone());
            let mut client = CrashClient::new(life_span, request);

            let settings = BrowserSettings {
                background_color: 0xFF1A1A1A,
                ..Default::default()
            };

            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(self.html.as_bytes());
            let data_url = format!("data:text/html;base64,{b64}");
            let url = CefString::from(data_url.as_str());

            let mut bv_delegate = CrashBrowserViewDelegate::new(0);
            let browser_view = browser_view_create(
                Some(&mut client),
                Some(&url),
                Some(&settings),
                None,
                None,
                Some(&mut bv_delegate),
            );

            let mut window_delegate =
                CrashWindowDelegate::new(RefCell::new(browser_view), self.sender.clone());
            window_create_top_level(Some(&mut window_delegate));
        }
    }
}

// --- Task to reload the main browser on the UI thread ---

wrap_task! {
    pub(crate) struct ReloadMainTask {
        _p: u8,
    }
    impl Task {
        fn execute(&self) {
            if let Some(browser) = crate::app_state::with_state(|s| s.browser.clone()).flatten() {
                browser.reload();
            }
        }
    }
}
