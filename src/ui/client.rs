use crate::app_state::{IpcMessage, exec_js_on_frame, open_in_os, with_state};
use crate::ipc::player::handle_ipc_message;
use crate::ipc::plugin::handle_plugin_ipc;
use cef::wrapper::message_router::{
    BrowserSideHandler, BrowserSideRouter, MessageRouterBrowserSideHandlerCallbacks,
};
use cef::*;
use std::cell::Cell;
use std::sync::{Arc, Mutex};

/// TIDAL app host that needs our runtime (init_script, early_runtime, auth).
fn is_tidal_app_host(url: &str) -> bool {
    url.contains("desktop.tidal.com")
        || url.contains("login.tidal.com")
        || url.contains("auth.tidal.com")
}

// Prepends a snippet to TIDAL's main JS bundle that exposes __webpack_require__
// globally, before the IIFE executes.

const WEBPACK_EXPOSE_SNIPPET: &str = r#"(function(){var _orig=Function.prototype.call;var _hooked=false;Function.prototype.call=function(){var r=_orig.apply(this,arguments);if(!_hooked&&typeof r==='function'&&r.m&&r.c){_hooked=true;self.__webpack_require__=r;self.__LUNAR_WEBPACK_CACHE__=r.c;Function.prototype.call=_orig;console.log('[luna:wp] __webpack_require__ captured, '+Object.keys(r.c).length+' modules')}return r};setTimeout(function(){Function.prototype.call=_orig},5000)})();
"#;

wrap_resource_request_handler! {
    struct TidalResourceRequestHandler {
        _p: u8,
    }
    impl ResourceRequestHandler {
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            if let Some(req) = request.as_ref() {
                let u = req.url();
                let url = format!("{}", CefString::from(&u));
                if is_tidal_app_host(&url) {
                    crate::vprintln!(
                        "[RRH]    on_before_resource_load: {}",
                        &url[..url.len().min(120)]
                    );
                }
            }
            ReturnValue::CONTINUE
        }
        fn resource_response_filter(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _response: Option<&mut Response>,
        ) -> Option<ResponseFilter> {
            let url = request
                .as_ref()
                .map(|r| {
                    let u = r.url();
                    format!("{}", CefString::from(&u))
                })
                .unwrap_or_default();
            if url.contains("desktop.tidal.com") && url.contains("/index-") && url.ends_with(".js")
            {
                crate::vprintln!("[WP-FILTER] Attaching response filter to: {}", url);
                Some(WebpackExposeFilter::new(Cell::new(false)))
            } else {
                None
            }
        }
    }
}

wrap_response_filter! {
    struct WebpackExposeFilter {
        injected: Cell<bool>,
    }
    impl ResponseFilter {
        fn init_filter(&self) -> ::std::os::raw::c_int {
            1
        }
        fn filter(
            &self,
            data_in: Option<&mut Vec<u8>>,
            data_in_read: Option<&mut usize>,
            data_out: Option<&mut Vec<u8>>,
            data_out_written: Option<&mut usize>,
        ) -> ResponseFilterStatus {
            let Some(input) = data_in else {
                if let Some(written) = data_out_written {
                    *written = 0;
                }
                return ResponseFilterStatus::DONE;
            };
            let Some(output) = data_out else {
                if let Some(read) = data_in_read {
                    *read = 0;
                }
                return ResponseFilterStatus::ERROR;
            };

            let snippet = if !self.injected.get() {
                self.injected.set(true);
                WEBPACK_EXPOSE_SNIPPET.as_bytes()
            } else {
                &[]
            };

            let needed = snippet.len() + input.len();
            if needed <= output.len() {
                // Everything fits
                output[..snippet.len()].copy_from_slice(snippet);
                output[snippet.len()..snippet.len() + input.len()].copy_from_slice(input);
                if let Some(read) = data_in_read {
                    *read = input.len();
                }
                if let Some(written) = data_out_written {
                    *written = snippet.len() + input.len();
                }
                ResponseFilterStatus::NEED_MORE_DATA
            } else if snippet.len() <= output.len() {
                // Snippet fits, partial input
                let avail = output.len() - snippet.len();
                output[..snippet.len()].copy_from_slice(snippet);
                output[snippet.len()..snippet.len() + avail].copy_from_slice(&input[..avail]);
                if let Some(read) = data_in_read {
                    *read = avail;
                }
                if let Some(written) = data_out_written {
                    *written = snippet.len() + avail;
                }
                ResponseFilterStatus::NEED_MORE_DATA
            } else {
                // Output buffer too small even for snippet — write what we can
                let out_len = output.len();
                output.copy_from_slice(&snippet[..out_len]);
                if let Some(read) = data_in_read {
                    *read = 0;
                }
                if let Some(written) = data_out_written {
                    *written = output.len();
                }
                ResponseFilterStatus::NEED_MORE_DATA
            }
        }
    }
}

// --- IPC Query Handler (JS -> Rust via cefQuery) ---

pub(super) struct IpcQueryHandler;

impl BrowserSideHandler for IpcQueryHandler {
    fn on_query_str(
        &self,
        _browser: Option<Browser>,
        _frame: Option<Frame>,
        _query_id: i64,
        request: &str,
        _persistent: bool,
        callback: Arc<Mutex<dyn cef::wrapper::message_router::BrowserSideCallback>>,
    ) -> bool {
        if let Ok(msg) = serde_json::from_str::<IpcMessage>(request)
            && (msg.channel.starts_with("plugin.")
                || msg.channel.starts_with("proxy.")
                || msg.channel.starts_with("jsrt.")
                || msg.channel.starts_with("__Luna.")
                || msg.channel.starts_with("__LunaNative.")
                || msg.channel == "player.parse_dash")
            && msg.id.is_some()
        {
            handle_plugin_ipc(msg, callback);
            return true;
        }

        handle_ipc_message(request);
        callback
            .lock()
            .expect("IPC callback lock poisoned")
            .success_str("ok");
        true
    }
}

// --- Drag Handler ---

wrap_drag_handler! {
    pub(super) struct TidalDragHandler {
        _p: u8,
    }
    impl DragHandler {
        fn on_draggable_regions_changed(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            regions: Option<&[DraggableRegion]>,
        ) {
            if let Some(bv) = browser_view_get_for_browser(browser)
                && let Some(window) = bv.window()
            {
                window.set_draggable_regions(regions);
            }
        }
    }
}

// --- Download Handler ---

wrap_download_handler! {
    pub(super) struct TidalDownloadHandler {
        _p: u8,
    }
    impl DownloadHandler {
        fn can_download(
            &self,
            _browser: Option<&mut Browser>,
            _url: Option<&CefString>,
            _request_method: Option<&CefString>,
        ) -> ::std::os::raw::c_int {
            0
        }
    }
}

// --- Permission Handler ---

wrap_permission_handler! {
    pub(super) struct TidalPermissionHandler {
        _p: u8,
    }
    impl PermissionHandler {
        fn on_request_media_access_permission(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _requesting_origin: Option<&CefString>,
            _requested_permissions: u32,
            callback: Option<&mut MediaAccessCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(cb) = callback {
                cb.cancel();
            }
            1
        }
        fn on_show_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            _prompt_id: u64,
            _requesting_origin: Option<&CefString>,
            _requested_permissions: u32,
            callback: Option<&mut PermissionPromptCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(cb) = callback {
                cb.cont(PermissionRequestResult::DENY);
            }
            1
        }
    }
}

// --- Client ---

wrap_client! {
    pub(super) struct TidalClient {
        life_span: LifeSpanHandler,
        load: LoadHandler,
        request: RequestHandler,
        display: DisplayHandler,
        drag: DragHandler,
        download: DownloadHandler,
        permission: PermissionHandler,
        router: Arc<BrowserSideRouter>,
    }
    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span.clone())
        }
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load.clone())
        }
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.request.clone())
        }
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display.clone())
        }
        fn drag_handler(&self) -> Option<DragHandler> {
            Some(self.drag.clone())
        }
        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(self.download.clone())
        }
        fn permission_handler(&self) -> Option<PermissionHandler> {
            Some(self.permission.clone())
        }
        fn on_process_message_received(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> i32 {
            if self.router.on_process_message_received(
                browser.cloned(),
                frame.cloned(),
                source_process,
                message.cloned(),
            ) {
                1
            } else {
                0
            }
        }
    }
}

// --- Life Span Handler ---

wrap_life_span_handler! {
    pub(super) struct TidalLifeSpanHandler {
        router: Arc<BrowserSideRouter>,
    }
    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: ::std::os::raw::c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            let url_dbg = target_url.as_ref().map(|u| u.to_string()).unwrap_or_default();
            crate::vprintln!("[POPUP]  on_before_popup: {}", &url_dbg[..url_dbg.len().min(120)]);
            if let Some(url) = target_url {
                let url_str = url.to_string();
                // Auth popup: configure a native window (not Views) for the login page
                if is_tidal_app_host(&url_str) && !url_str.contains("desktop.tidal.com") {
                    crate::vprintln!("[AUTH]   Opening auth popup: {}", &url_str[..url_str.len().min(120)]);
                    if let Some(wi) = _window_info {
                        wi.window_name = CefString::from("TidaLunar - Login");
                        wi.bounds = cef::Rect { x: 100, y: 100, width: 500, height: 700 };
                        #[cfg(target_os = "windows")]
                        {
                            wi.style = 0x00CF0000; // WS_OVERLAPPEDWINDOW
                        }
                        wi.runtime_style = RuntimeStyle::DEFAULT;
                    }
                    return 0;
                }
                if (url_str.starts_with("http://") || url_str.starts_with("https://"))
                    && !is_tidal_app_host(&url_str)
                {
                    open_in_os(&url_str);
                }
            }
            1
        }
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            if let Some(browser) = browser.cloned() {
                let is_popup = browser.is_popup() != 0;
                crate::vprintln!("[CEF]    on_after_created: popup={}", is_popup);
                if !is_popup {
                    // Only store the main browser, not auth popups
                    with_state(|state| {
                        state.browser = Some(browser);
                    });
                }
            }
        }
        fn do_close(&self, _browser: Option<&mut Browser>) -> i32 {
            0
        }
        fn on_before_close(&self, browser: Option<&mut Browser>) {
            let is_popup = browser
                .as_ref()
                .map(|b| b.is_popup() != 0)
                .unwrap_or(false);
            crate::vprintln!("[CEF]    on_before_close: popup={}", is_popup);
            self.router.on_before_close(browser.cloned());
            if !is_popup {
                // Only clear state and quit for the main browser
                with_state(|state| {
                    state.browser = None;
                });
                quit_message_loop();
            }
        }
    }
}

// --- Load Handler ---

#[derive(Clone, Copy, PartialEq)]
pub(super) enum PageState {
    Initial,
    App,
    Login,
}

wrap_load_handler! {
    pub(super) struct TidalLoadHandler {
        init_script: String,
        bundle_script: String,
        page_state: Cell<PageState>,
    }
    impl LoadHandler {
        fn on_load_start(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _transition_type: TransitionType,
        ) {
            if let Some(frame) = frame {
                let url_userfree = frame.url();
                let url = format!("{}", CefString::from(&url_userfree));
                if is_tidal_app_host(&url) {
                    crate::vprintln!("[LOAD]   on_load_start init_script: {}", &url[..url.len().min(80)]);
                    exec_js_on_frame(frame, &self.init_script);
                }
            }
        }
        fn on_loading_state_change(
            &self,
            browser: Option<&mut Browser>,
            is_loading: i32,
            _can_go_back: i32,
            _can_go_forward: i32,
        ) {
            if is_loading == 0
                && let Some(browser) = browser
                && let Some(frame) = browser.main_frame()
            {
                let url_userfree = frame.url();
                let url = format!("{}", CefString::from(&url_userfree));
                if is_tidal_app_host(&url) && !url.contains("desktop.tidal.com") {
                    crate::vprintln!("[LOAD]   Auth page loaded: {}", &url[..url.len().min(100)]);
                }
                if !url.contains("desktop.tidal.com") {
                    return;
                }

                // /login is the login page; /login/auth is the OAuth callback (not a login page)
                let is_login = url.contains("/login") && !url.contains("/login/auth");
                let prev = self.page_state.get();

                // Transitioning from login to app — stop the player but don't reload.
                if prev == PageState::Login && !is_login {
                    with_state(|state| {
                        let _ = state.player.stop();
                        state.pending_player_events.clear();
                        state.pending_time_update = None;
                    });
                    crate::vprintln!("[LOAD]   Post-login transition to app");
                }

                if prev != PageState::Initial && is_login {
                    with_state(|state| {
                        let _ = state.player.stop();
                    });
                }
                self.page_state.set(if is_login {
                    PageState::Login
                } else {
                    PageState::App
                });
                exec_js_on_frame(
                    &frame,
                    &format!(
                        "if(!window.__TL_INJECTED__){{window.__TL_INJECTED__=true;{}}}",
                        self.bundle_script
                    ),
                );
            }
        }
        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            let url = failed_url.map(|u| u.to_string()).unwrap_or_default();
            let text = error_text.map(|t| t.to_string()).unwrap_or_default();
            crate::vprintln!("[LOAD]   on_load_error: {} {:?} {}", url, error_code, text);
        }
    }
}

// --- Request Handler ---

wrap_request_handler! {
    pub(super) struct TidalRequestHandler {
        router: Arc<BrowserSideRouter>,
    }
    impl RequestHandler {
        fn on_before_browse(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _user_gesture: ::std::os::raw::c_int,
            _is_redirect: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let url = request
                .as_ref()
                .map(|r| {
                    let u = r.url();
                    format!("{}", CefString::from(&u))
                })
                .unwrap_or_default();

            crate::vprintln!(
                "[NAV]    on_before_browse: is_redirect={} url={}",
                _is_redirect,
                &url[..url.len().min(200)]
            );

            if url.starts_with("tidal://") {
                let web_url = url.replacen("tidal://", "https://desktop.tidal.com/", 1);
                crate::vprintln!("[AUTH]   Intercepted tidal:// redirect → {}", web_url);
                with_state(|state| {
                    if let Some(ref browser) = state.browser
                        && let Some(main_frame) = browser.main_frame()
                    {
                        let cef_url = CefString::from(web_url.as_str());
                        main_frame.load_url(Some(&cef_url));
                    }
                });
                // Only close popup browsers, not the main window
                if let Some(browser) = browser {
                    if browser.is_popup() != 0 {
                        crate::vprintln!("[AUTH]   Closing auth popup after tidal:// redirect");
                        if let Some(host) = browser.host() {
                            host.try_close_browser();
                        }
                    } else {
                        crate::vprintln!("[AUTH]   Main frame redirect, not closing browser");
                    }
                }
                return 1;
            }

            // Skip router for auth navigations — the router's on_before_browse
            // can interfere with cross-origin navigation to login/auth hosts
            if is_tidal_app_host(&url) && !url.contains("desktop.tidal.com") {
                crate::vprintln!("[AUTH]   Bypassing router for auth navigation");
                return 0;
            }

            self.router
                .on_before_browse(browser.cloned(), frame.cloned());
            0
        }
        fn on_certificate_error(
            &self,
            _browser: Option<&mut Browser>,
            _cert_error: Errorcode,
            _request_url: Option<&CefString>,
            _ssl_info: Option<&mut Sslinfo>,
            _callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            0
        }
        fn resource_request_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int,
            _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            if let Some(req) = request.as_ref() {
                let u = req.url();
                let url = format!("{}", CefString::from(&u));
                // Don't attach resource handler to auth hosts — it can interfere with navigation
                if is_tidal_app_host(&url) && !url.contains("desktop.tidal.com") {
                    crate::vprintln!("[RRH]    Skipping handler for auth host: {}", &url[..url.len().min(100)]);
                    return None;
                }
                if url.contains("/index-") || url.contains("/assets/") {
                    crate::vprintln!("[WP-RRH] Request: {}", &url[..url.len().min(120)]);
                }
            }
            Some(TidalResourceRequestHandler::new(0))
        }
        fn on_render_process_terminated(
            &self,
            browser: Option<&mut Browser>,
            _status: TerminationStatus,
            _error_code: ::std::os::raw::c_int,
            _error_string: Option<&CefString>,
        ) {
            self.router
                .on_render_process_terminated(browser.cloned());
        }
    }
}

// --- Display Handler ---

wrap_display_handler! {
    pub(super) struct TidalDisplayHandler {
        _p: u8,
    }
    impl DisplayHandler {
        fn on_title_change(&self, browser: Option<&mut Browser>, title: Option<&CefString>) {
            let mut browser = browser.cloned();
            if let Some(bv) = browser_view_get_for_browser(browser.as_mut())
                && let Some(window) = bv.window()
            {
                window.set_title(title);
            }
        }
        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            _level: LogSeverity,
            message: Option<&CefString>,
            _source: Option<&CefString>,
            _line: i32,
        ) -> i32 {
            if let Some(msg) = message {
                let s = msg.to_string();
                if let Some(json) = s.strip_prefix("__IPC__:") {
                    handle_ipc_message(json);
                    return 0;
                }
                if s.starts_with("[DBG:") || s.starts_with("[withFormat") {
                    crate::vprintln3!("[JS] {s}");
                } else {
                    crate::vprintln!("[JS] {s}");
                }
            }
            0
        }
    }
}
