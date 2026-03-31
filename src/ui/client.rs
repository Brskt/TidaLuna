use crate::app_state::{IpcMessage, exec_js_on_frame, open_in_os, with_state};
use crate::ipc::player::handle_ipc_message;
use crate::ipc::plugin::handle_plugin_ipc;
use crate::ui::nav::{self, NavigationPolicy, PageKind};
use cef::wrapper::message_router::{
    BrowserSideHandler, BrowserSideRouter, MessageRouterBrowserSideHandlerCallbacks,
};
use cef::*;
use std::cell::Cell;
use std::sync::{Arc, Mutex};

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
                || msg.channel.starts_with("tidal.")
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
            let allowed = _url
                .map(|u| u.to_string().starts_with("blob:"))
                .unwrap_or(false);
            if allowed { 1 } else { 0 }
        }
        fn on_before_download(
            &self,
            _browser: Option<&mut Browser>,
            _download_item: Option<&mut DownloadItem>,
            _suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(cb) = callback {
                let empty = CefString::from("");
                cb.cont(Some(&empty), 1);
                return 1;
            }
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
            let url_str = target_url.as_ref().map(|u| u.to_string()).unwrap_or_default();
            crate::vprintln!("[POPUP]  on_before_popup: {}", &url_str[..url_str.len().min(120)]);
            if target_url.is_some() {
                let kind = PageKind::classify(&url_str);
                if kind == PageKind::AuthHost {
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
                    && kind == PageKind::External
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
                let url = crate::ui::token_filter::userfree_to_string(&url_userfree);
                let policy = NavigationPolicy::for_page(PageKind::classify(&url));
                if policy.inject_init_script {
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
            crate::vprintln!(
                "[LOAD]   on_loading_state_change: is_loading={} can_go_back={} can_go_forward={}",
                is_loading, _can_go_back, _can_go_forward
            );
            if is_loading == 0
                && let Some(browser) = browser
                && let Some(frame) = browser.main_frame()
            {
                let url_userfree = frame.url();
                let url = crate::ui::token_filter::userfree_to_string(&url_userfree);
                let kind = PageKind::classify(&url);
                let policy = NavigationPolicy::for_page(kind);
                if kind == PageKind::AuthHost {
                    crate::vprintln!("[LOAD]   Auth page loaded: {}", &url[..url.len().min(100)]);
                }
                if !policy.inject_bundle {
                    return;
                }

                let is_login = kind == PageKind::LoginPage;
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
            crate::vprintln!("[LOAD]   on_load_error: url={} code={:?} text={}", url, error_code, text);
            let is_main = _frame.as_ref().map(|f| f.is_main() != 0).unwrap_or(false);
            crate::vprintln!("[LOAD]   on_load_error: is_main_frame={}", is_main);
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
                    crate::ui::token_filter::userfree_to_string(&u)
                })
                .unwrap_or_default();

            let kind = PageKind::classify(&url);
            let policy = NavigationPolicy::for_page(kind);

            crate::vprintln!(
                "[NAV]    on_before_browse: is_redirect={} url={}",
                _is_redirect,
                &url[..url.len().min(200)]
            );

            if kind == PageKind::TidalCallback {
                let web_url = url.replacen("tidal://", &format!("https://{}/", nav::HOST_DESKTOP), 1);
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

            if policy.bypass_router {
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
            _request: Option<&mut Request>,
            _is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int,
            _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            let url = _request
                .as_ref()
                .map(|r| {
                    let u = r.url();
                    crate::ui::token_filter::userfree_to_string(&u)
                })
                .unwrap_or_default();
            if crate::ui::token_filter::should_rewrite_token(&url)
                || crate::ui::nav::is_token_endpoint(&url)
            {
                Some(crate::ui::token_filter::TokenResourceHandler::new())
            } else {
                None
            }
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
