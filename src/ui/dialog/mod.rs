//! Shared native CEF Views dialog. Each dialog (trust / install / crash / message) is a
//! separate top-level window isolated from the main renderer: its buttons navigate to a
//! custom-scheme URL (`trust://`, `install://`, `crash://`, `msgbox://`) which
//! the dialog's own RequestHandler intercepts and maps to a result. This module
//! owns the shared skeleton (client, delegates, the UI-thread task, centering and the ALLOY
//! runtime style); each caller supplies only its HTML, size, a `url -> Option<R>` parser and
//! the result to send if the window is closed without a button.

use cef::*;
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use tokio::sync::oneshot;

use crate::util::reply::OnceReply;

mod html;
mod placement;

pub(crate) use html::{author_block, escape_html};
use placement::{center_on_parent, dialog_parent};

const FIT_SCHEME: &str = "dialog-size://";

/// Goes last in `<body>`, where the layout it reads is complete; the window then shrinks to the
/// body's height, its size given to `show_dialog` becoming a ceiling.
pub(crate) const FIT_TO_CONTENT: &str = "<script>location.href='dialog-size://'+Math.ceil(document.body.getBoundingClientRect().height)</script>";

fn parse_fit_height(url: &str) -> Option<i32> {
    url.strip_prefix(FIT_SCHEME)?
        .trim_end_matches('/')
        .parse::<i32>()
        .ok()
        .filter(|h| *h > 0)
}

/// `current` with its height set to `measured`, capped at `ceiling`, and the same vertical center.
fn fitted_bounds(current: Rect, measured: i32, ceiling: i32) -> Rect {
    let height = measured.min(ceiling);
    Rect {
        y: current.y + (current.height - height) / 2,
        height,
        ..current
    }
}

/// Https only, whatever the resource type: the frame is a `data:` document, and Blink
/// restricts mixed content only in an `https` one. The image gate judges images alone; this
/// rule covers the other types a dialog could load.
fn dialog_subresource_allowed(url: &crate::ui::nav::RequestUrl) -> bool {
    url.parsed()
        .is_some_and(|parsed| parsed.scheme() == "https")
}

/// Result-specific state shared by the dialog's request handler, window
/// delegate, and UI-thread task. A single `Arc<DialogCore<R>>` reaches both the
/// request handler (button click) and the window delegate (close-without-click).
/// Whichever of them answers first settles `sender`.
struct DialogCore<R> {
    sender: OnceReply<R>,
    /// Maps a button-navigation URL to its result, or `None` for any other nav.
    parse: fn(&str) -> Option<R>,
    /// Result sent when the window is closed without a button click.
    on_close: R,
    width: i32,
    height: i32,
    /// `height` until the page reports its own through `FIT_TO_CONTENT`.
    fitted_height: AtomicI32,
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
        sender: OnceReply::new(tx),
        parse,
        on_close,
        width: size.0,
        height: size.1,
        fitted_height: AtomicI32::new(size.1),
    });
    let mut task = ShowDialogTask::new(html, core);
    post_task(ThreadId::UI, Some(&mut task));
    rx
}

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

            if let Some(measured) = parse_fit_height(&url) {
                let window = browser_view_get_for_browser(browser).and_then(|bv| bv.window());
                if let Some(window) = window {
                    let bounds = fitted_bounds(window.bounds(), measured, self.core.height);
                    self.core.fitted_height.store(bounds.height, Ordering::Relaxed);
                    window.set_bounds(Some(&bounds));
                }
                return 1;
            }
            if let Some(result) = (self.core.parse)(&url) {
                self.core.sender.send(result);
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
            request: Option<&mut Request>,
            is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int,
            request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            // The trust prompt interpolates an avatar URL straight out of a plugin's
            // manifest, and this window's frame is a `data:` URL, which PageKind reads as
            // External: no early runtime is injected here. This handler is the only layer
            // that ever sees the request, so the gate has to live here or nowhere.
            let url = crate::ui::nav::RequestUrl::new(
                request
                    .as_ref()
                    .map(|r| crate::ui::token_filter::userfree_to_string(&r.url()))
                    .unwrap_or_default(),
            );
            // Navigations are the buttons' schemes, which `on_before_browse` owns.
            if is_navigation == 0 && !dialog_subresource_allowed(&url) {
                crate::verr!(
                    "[EXFIL]  BLOCKED dialog non-https request to {}",
                    crate::util::refused_host(url.as_str())
                );
                return Some(crate::ui::exfil_guard::ExfilBlockHandler::new());
            }
            if let Some(req) = request.as_ref()
                && let Some(h) = crate::ui::exfil_guard::block_disallowed_image(
                    req.resource_type(),
                    &url,
                    "dialog",
                    request_initiator,
                )
            {
                return Some(h);
            }
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

wrap_life_span_handler! {
    struct DialogLifeSpanHandler {
        _p: u8,
    }
    impl LifeSpanHandler {}
}

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

wrap_window_delegate! {
    struct DialogWindowDelegate<R: Clone + Send + Sync + 'static> {
        browser_view: RefCell<Option<BrowserView>>,
        core: Arc<DialogCore<R>>,
    }
    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            Size {
                width: self.core.width,
                height: self.core.fitted_height.load(Ordering::Relaxed),
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
        fn parent_window(
            &self,
            _window: Option<&mut Window>,
            _is_menu: Option<&mut ::std::os::raw::c_int>,
            _can_activate_menu: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<Window> {
            dialog_parent()
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
                let png_data = include_bytes!("../../../tidaluna.png");
                icon.add_png(1.0, Some(png_data));
                window.set_window_icon(Some(&mut icon));
            }

            window.show();
            window.activate();
        }
        fn on_window_destroyed(&self, _window: Option<&mut Window>) {
            *self.browser_view.borrow_mut() = None;
            // Closing via X without clicking a button -> the caller's default.
            self.core.sender.send(self.core.on_close.clone());
        }
        fn on_window_activation_changed(
            &self,
            _window: Option<&mut Window>,
            active: ::std::os::raw::c_int,
        ) {
            crate::vprintln!("[DIALOG] activation changed: active={}", active != 0);
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
#[path = "../../../tests/unit/ui/dialog.rs"]
mod tests;
