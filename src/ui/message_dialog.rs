//! Native message/error dialog - separate CEF window, isolated from the main
//! renderer (same model as `trust_dialog`). Button clicks navigate to
//! `msgbox://<index>`, intercepted by the dialog's own RequestHandler; the
//! chosen index is returned via a oneshot channel. Backs `showMessageBox` and
//! `showErrorBox` from `@luna/lib.native`.

use super::trust_dialog::escape_html;
use cef::*;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

const MSGBOX_SCHEME: &str = "msgbox://";
const DIALOG_W: i32 = 460;
const DIALOG_H: i32 = 240;

/// Show a message dialog and return the clicked button index via a oneshot.
/// `cancel_id` is returned if the window is closed without a button click.
/// Can be called from any thread - internally posts to the CEF UI thread.
pub(crate) fn show_message_dialog(
    title: &str,
    message: &str,
    detail: &str,
    buttons: &[String],
    default_id: i32,
    cancel_id: i32,
) -> oneshot::Receiver<i32> {
    let (tx, rx) = oneshot::channel();
    let html = build_html(title, message, detail, buttons, default_id);
    let sender = Arc::new(Mutex::new(Some(tx)));
    let mut task = ShowDialogTask::new(html, sender, cancel_id);
    post_task(ThreadId::UI, Some(&mut task));
    rx
}

fn build_html(
    title: &str,
    message: &str,
    detail: &str,
    buttons: &[String],
    default_id: i32,
) -> String {
    let title_html = if title.is_empty() {
        String::new()
    } else {
        format!(r#"<h2>{}</h2>"#, escape_html(title))
    };

    let message_html = if message.is_empty() {
        String::new()
    } else {
        format!(r#"<div class="message">{}</div>"#, escape_html(message))
    };

    let detail_html = if detail.is_empty() {
        String::new()
    } else {
        format!(r#"<div class="detail">{}</div>"#, escape_html(detail))
    };

    let buttons_html = buttons
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let cls = if i as i32 == default_id {
                "btn primary"
            } else {
                "btn"
            };
            format!(
                r#"<button class="{cls}" onclick="location.href='{scheme}{i}'">{label}</button>"#,
                cls = cls,
                scheme = MSGBOX_SCHEME,
                i = i,
                label = escape_html(label)
            )
        })
        .collect::<Vec<_>>()
        .join("");

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
.dialog {{ max-width:420px; width:90%; -webkit-app-region:no-drag; }}
h2 {{ font-size:16px; margin-bottom:12px; }}
.message {{ font-size:14px; color:#eee; line-height:1.5; margin-bottom:8px; }}
.detail {{ font-size:12px; color:#999; line-height:1.5; margin-bottom:16px; }}
.actions {{ display:flex; gap:8px; justify-content:flex-end; margin-top:16px; flex-wrap:wrap; }}
button {{
    padding:8px 16px; border:none; border-radius:4px;
    color:#fff; cursor:pointer; font-size:13px; background:#333;
}}
button:hover {{ background:#444; }}
button.primary {{ background:#eb1e32; }}
button.primary:hover {{ background:#d11a2d; }}
</style>
</head>
<body>
<div class="dialog">
    {title_html}
    {message_html}
    {detail_html}
    <div class="actions">{buttons_html}</div>
</div>
</body>
</html>"#,
        title_html = title_html,
        message_html = message_html,
        detail_html = detail_html,
        buttons_html = buttons_html,
    )
}

// --- Request handler: intercepts msgbox:// navigations ---

wrap_request_handler! {
    struct DialogRequestHandler {
        sender: Arc<Mutex<Option<oneshot::Sender<i32>>>>,
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

            if let Some(index) = url.strip_prefix(MSGBOX_SCHEME).and_then(|s| s.parse::<i32>().ok()) {
                if let Some(tx) = self.sender.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = tx.send(index);
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
    struct MessageDialogWindowDelegate {
        browser_view: RefCell<Option<BrowserView>>,
        sender: Arc<Mutex<Option<oneshot::Sender<i32>>>>,
        cancel_id: i32,
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
            // Closing via X without clicking a button → cancel.
            if let Some(tx) = self.sender.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = tx.send(self.cancel_id);
            }
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
    struct ShowDialogTask {
        html: String,
        sender: Arc<Mutex<Option<oneshot::Sender<i32>>>>,
        cancel_id: i32,
    }
    impl Task {
        fn execute(&self) {
            let life_span = DialogLifeSpanHandler::new(0);
            let request = DialogRequestHandler::new(self.sender.clone());
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

            let mut window_delegate = MessageDialogWindowDelegate::new(
                RefCell::new(browser_view),
                self.sender.clone(),
                self.cancel_id,
            );
            window_create_top_level(Some(&mut window_delegate));
        }
    }
}
