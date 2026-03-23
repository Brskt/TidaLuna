use crate::app_state::{IpcMessage, exec_js_on_frame, open_in_os, with_state};
use crate::ipc::player::handle_ipc_message;
use crate::ipc::plugin::handle_plugin_ipc;
use crate::ipc::window::notify_window_state;
use crate::ui::flush::run_flush_batch;
use cef::wrapper::message_router::{
    BrowserSideHandler, BrowserSideRouter, MessageRouterBrowserSide,
    MessageRouterBrowserSideHandlerCallbacks, MessageRouterConfig,
    MessageRouterRendererSideHandlerCallbacks, RendererSideRouter,
};
use cef::*;
use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex};

// --- IPC Query Handler (JS -> Rust via cefQuery) ---

struct IpcQueryHandler;

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
    struct TidalDragHandler {
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
    struct TidalDownloadHandler {
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
    struct TidalPermissionHandler {
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
    struct TidalClient {
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
    struct TidalLifeSpanHandler {
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
            if let Some(url) = target_url {
                let url_str = url.to_string();
                if (url_str.starts_with("http://") || url_str.starts_with("https://"))
                    && !url_str.contains("desktop.tidal.com")
                {
                    open_in_os(&url_str);
                }
            }
            1
        }
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            if let Some(browser) = browser.cloned() {
                with_state(|state| {
                    state.browser = Some(browser);
                });
            }
        }
        fn do_close(&self, _browser: Option<&mut Browser>) -> i32 {
            0
        }
        fn on_before_close(&self, browser: Option<&mut Browser>) {
            self.router.on_before_close(browser.cloned());
            with_state(|state| {
                state.browser = None;
            });
            quit_message_loop();
        }
    }
}

// --- Load Handler ---

#[derive(Clone, Copy, PartialEq)]
enum PageState {
    Initial,
    App,
    Login,
}

wrap_load_handler! {
    struct TidalLoadHandler {
        init_script: String,
        bundle_script: String,
        page_state: Cell<PageState>,
    }
    impl LoadHandler {
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
                if !url.contains("desktop.tidal.com") {
                    return;
                }

                let is_login = url.contains("/login");
                let prev = self.page_state.get();

                if prev == PageState::Login && !is_login {
                    self.page_state.set(PageState::Initial);
                    with_state(|state| {
                        let _ = state.player.stop();
                        state.pending_player_events.clear();
                        state.pending_time_update = None;
                    });
                    crate::vprintln!("[LOAD]   Post-login redirect detected, reloading");
                    browser.reload();
                    return;
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
                exec_js_on_frame(&frame, &self.init_script);
                exec_js_on_frame(
                    &frame,
                    &format!(
                        "if(!window.__TL_INJECTED__){{window.__TL_INJECTED__=true;{}}}",
                        self.bundle_script
                    ),
                );
            }
        }
    }
}

// --- Request Handler ---

wrap_request_handler! {
    struct TidalRequestHandler {
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
            if let Some(req) = request {
                let url_userfree = req.url();
                let url_cef: CefString = CefString::from(&url_userfree);
                let url = format!("{}", url_cef);
                if url.starts_with("tidal://") {
                    let web_url = url.replacen("tidal://", "https://desktop.tidal.com/", 1);
                    crate::vprintln!("[AUTH]   Intercepted tidal:// redirect → {}", web_url);
                    if let Some(frame) = frame {
                        let cef_url = CefString::from(web_url.as_str());
                        frame.load_url(Some(&cef_url));
                    }
                    return 1;
                }
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
    struct TidalDisplayHandler {
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

// --- Win32 frameless subclass ---

#[cfg(target_os = "windows")]
unsafe extern "system" fn frameless_subclass_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
    _uid: usize,
    _ref_data: usize,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::Shell::DefSubclassProc;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    unsafe {
        match msg {
            WM_NCCALCSIZE if wparam != 0 => {
                let params = &*(lparam as *const NCCALCSIZE_PARAMS);
                let proposed = params.rgrc[0];
                DefWindowProcW(hwnd, msg, wparam, lparam);
                let params = &mut *(lparam as *mut NCCALCSIZE_PARAMS);

                if IsZoomed(hwnd) != 0 {
                    use windows_sys::Win32::Graphics::Gdi::*;
                    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                    let mut mi: MONITORINFO = core::mem::zeroed();
                    mi.cbSize = core::mem::size_of::<MONITORINFO>() as u32;
                    if GetMonitorInfoW(monitor, &mut mi) != 0 {
                        params.rgrc[0] = mi.rcWork;
                    } else {
                        params.rgrc[0] = proposed;
                    }
                } else {
                    params.rgrc[0] = proposed;
                }
                0
            }
            WM_SYSCOMMAND
                if matches!(
                    (wparam & 0xFFF0) as u32,
                    SC_MINIMIZE | SC_MAXIMIZE | SC_RESTORE
                ) =>
            {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_COMMAND => {
                let id = (wparam & 0xFFFF) as u32;
                match id {
                    0 => crate::app_state::eval_js(
                        "window.__TIDAL_PLAYBACK_DELEGATE__?.playPrevious?.()",
                    ),
                    1 => crate::app_state::eval_js("window.__TL_PLAY_PAUSE__?.()"),
                    2 => crate::app_state::eval_js(
                        "window.__TIDAL_PLAYBACK_DELEGATE__?.playNext?.()",
                    ),
                    _ => return DefSubclassProc(hwnd, msg, wparam, lparam),
                }
                0
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }
}

// --- Window Delegate ---

wrap_window_delegate! {
    struct TidalWindowDelegate {
        browser_view: RefCell<Option<BrowserView>>,
    }
    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            let ws = with_state(|state| state.app_settings.load_window_state())
                .unwrap_or_default();
            Size {
                width: ws.width as i32,
                height: ws.height as i32,
            }
        }
    }
    impl PanelDelegate {}
    impl WindowDelegate {
        fn initial_bounds(&self, _window: Option<&mut Window>) -> cef::Rect {
            let ws = with_state(|state| state.app_settings.load_window_state())
                .unwrap_or_default();
            if ws.has_position() && !ws.maximized {
                cef::Rect {
                    x: ws.x,
                    y: ws.y,
                    width: ws.width as i32,
                    height: ws.height as i32,
                }
            } else {
                Default::default()
            }
        }
        fn initial_show_state(&self, _window: Option<&mut Window>) -> cef::ShowState {
            let ws = with_state(|state| state.app_settings.load_window_state())
                .unwrap_or_default();
            if ws.maximized {
                cef::ShowState::MAXIMIZED
            } else {
                cef::ShowState::NORMAL
            }
        }
        fn is_frameless(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            1
        }
        fn can_resize(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            1
        }
        fn can_maximize(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            1
        }
        fn can_minimize(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            1
        }
        fn on_window_created(&self, window: Option<&mut Window>) {
            let bv = self.browser_view.borrow();
            let (Some(window), Some(bv)) = (window, bv.as_ref()) else {
                return;
            };
            let mut view = View::from(bv);
            window.add_child_view(Some(&mut view));

            if let Some(mut icon) = image_create() {
                let png_data = include_bytes!("../../tidaluna.png");
                icon.add_png(1.0, Some(png_data));
                window.set_window_icon(Some(&mut icon));
                window.set_window_app_icon(Some(&mut icon));
            }

            #[cfg(target_os = "windows")]
            {
                use windows_sys::Win32::UI::Shell::SetWindowSubclass;
                use windows_sys::Win32::UI::WindowsAndMessaging::*;
                let hwnd = window.window_handle().0 as *mut core::ffi::c_void;
                if !hwnd.is_null() {
                    unsafe {
                        let style = GetWindowLongPtrW(hwnd as _, GWL_STYLE) as u32;
                        let style = (style & !WS_POPUP)
                            | WS_THICKFRAME
                            | WS_MINIMIZEBOX
                            | WS_MAXIMIZEBOX
                            | WS_SYSMENU;
                        SetWindowLongPtrW(hwnd as _, GWL_STYLE, style as isize);
                        SetWindowSubclass(hwnd, Some(frameless_subclass_proc), 1, 0);
                        SetWindowPos(
                            hwnd,
                            core::ptr::null_mut(),
                            0,
                            0,
                            0,
                            0,
                            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
                        );
                    }
                }
            }

            {
                #[cfg(target_os = "windows")]
                let hwnd = Some(window.window_handle().0 as *mut std::ffi::c_void);
                #[cfg(not(target_os = "windows"))]
                let hwnd = None;

                if let Some(mc) = crate::platform::media_controls::OsMediaControls::new(hwnd) {
                    with_state(|state| {
                        state.media_controls = Some(mc);
                    });
                }
            }

            window.show();

            #[cfg(target_os = "windows")]
            {
                let hwnd = window.window_handle().0 as windows_sys::Win32::Foundation::HWND;
                if let Some(tb) = crate::platform::thumbbar::ThumbBar::new(hwnd) {
                    with_state(|state| {
                        state.thumbbar = Some(tb);
                    });
                }
            }
        }
        fn on_window_destroyed(&self, _window: Option<&mut Window>) {
            *self.browser_view.borrow_mut() = None;
        }
        fn on_window_bounds_changed(
            &self,
            window: Option<&mut Window>,
            new_bounds: Option<&cef::Rect>,
        ) {
            let (Some(window), Some(bounds)) = (window, new_bounds) else {
                return;
            };
            if window.is_minimized() == 1 {
                return;
            }
            let maximized = window.is_maximized() == 1;
            let batch = with_state(|state| {
                state.pending_window_save = Some(crate::settings::WindowState {
                    x: bounds.x,
                    y: bounds.y,
                    width: bounds.width as u32,
                    height: bounds.height as u32,
                    maximized,
                });
                if !state.window_save_scheduled {
                    state.window_save_scheduled = true;
                    schedule_window_save();
                }
                notify_window_state(state, maximized, false)
            });
            if let Some(batch) = batch {
                run_flush_batch(batch);
            }
        }
        fn can_close(&self, _window: Option<&mut Window>) -> i32 {
            with_state(|state| {
                if let Some(ws) = state.pending_window_save.take() {
                    if ws.maximized {
                        state.app_settings.save_maximized(true);
                    } else {
                        state.app_settings.save_window_state(&ws);
                    }
                }
            });
            let bv = self.browser_view.borrow();
            let bv = bv.as_ref().expect("BrowserView is None");
            if let Some(browser) = bv.browser() {
                browser.host().unwrap().try_close_browser()
            } else {
                1
            }
        }
    }
}

// --- Browser View Delegate ---

wrap_browser_view_delegate! {
    struct TidalBrowserViewDelegate {
        _p: u8,
    }
    impl ViewDelegate {}
    impl BrowserViewDelegate {}
}

// --- Render Process Handler ---

wrap_render_process_handler! {
    struct TidalRenderProcessHandler {
        router: Arc<RendererSideRouter>,
    }
    impl RenderProcessHandler {
        fn on_context_created(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            self.router
                .on_context_created(browser.cloned(), frame.cloned(), context.cloned());
        }
        fn on_context_released(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            self.router
                .on_context_released(browser.cloned(), frame.cloned(), context.cloned());
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
                Some(source_process),
                message.cloned(),
            ) {
                1
            } else {
                0
            }
        }
    }
}

// --- App ---

const CEF_SWITCHES: &[&str] = &[
    "disable-background-networking",
    "disable-sync",
    "disable-default-apps",
    "disable-component-update",
    "disable-breakpad",
    "disable-crash-reporter",
    "disable-extensions",
    "disable-translate",
    "disable-notifications",
    "disable-spell-checking",
    "disable-client-side-phishing-detection",
    "disable-web-security",
    "no-first-run",
    "no-default-browser-check",
    "disable-hang-monitor",
    "disable-popup-blocking",
    "disable-prompt-on-repost",
    "disable-save-password-bubble",
    "disable-webrtc",
    "site-per-process",
    "disable-accelerated-video-decode",
    "disable-accelerated-video-encode",
];

const CEF_DISABLED_FEATURES: &[&str] = &[
    "PasswordManager",
    "PasswordManagerOnboarding",
    "AutofillServerCommunication",
    "AutofillCreditCardEnabled",
    "AutofillProfileEnabled",
    "KeyboardAccessory",
    "Translate",
    "TranslateUI",
    "WebPayments",
    "PaymentHandler",
    "SecurePaymentConfirmation",
    "DigitalGoodsAPI",
    "WebUSB",
    "WebBluetooth",
    "WebHID",
    "WebNFC",
    "WebMidi",
    "Serial",
    "Gamepad",
    "GenericSensorExtraClasses",
    "AmbientLightSensor",
    "BackgroundFetch",
    "BackgroundSync",
    "PushMessaging",
    "IdleDetection",
    "GetDisplayMedia",
    "HardwareMediaKeyHandling",
    "SpeechSynthesis",
    "SpeechRecognition",
    "WebRtcHideLocalIpsWithMdns",
    "WebXR",
    "Topics",
    "Fledge",
    "FledgeInterestGroupAPI",
    "AttributionReporting",
    "PrivateAggregation",
    "SharedStorageAPI",
    "PrivacySandboxAdsAPIsOverride",
    "FedCm",
    "FedCmAutoSigninAPI",
    "WebOTP",
    "ClientHints",
    "UserAgentClientHint",
    "MetricsReportingPolicy",
    "ReportingAPI",
    "DeprecationReporting",
    "SafeBrowsing",
    "InterestFeedContentSuggestions",
    "FileSystemAccess",
    "ComponentUpdate",
    "DirectSockets",
    "EyeDropper",
    "WindowPlacement",
    "ContactsPicker",
    "ContentIndex",
    "InstalledApp",
    "PictureInPictureV2",
    "BackForwardCache",
    "SpareRendererForSitePerProcess",
    "GlobalMediaControls",
    "MediaRouter",
    "OptimizationHints",
    "CalculateNativeWinOcclusion",
];

const CEF_ENABLED_FEATURES: &[&str] = &["PartitionAllocMemoryReclaimer"];

wrap_app! {
    pub(crate) struct TidalApp {
        renderer_router: Arc<RendererSideRouter>,
    }
    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let proc_type = _process_type
                .map(|s| format!("{}", s))
                .unwrap_or_default();
            if !proc_type.is_empty() {
                return;
            }

            let Some(cmd) = command_line else { return };

            for s in CEF_SWITCHES {
                let name = CefString::from(*s);
                cmd.append_switch(Some(&name));
            }

            let switch = format!("disable-features={}", CEF_DISABLED_FEATURES.join(","));
            let switch_cef = CefString::from(switch.as_str());
            cmd.append_switch(Some(&switch_cef));

            let enable = format!("enable-features={}", CEF_ENABLED_FEATURES.join(","));
            let enable_cef = CefString::from(enable.as_str());
            cmd.append_switch(Some(&enable_cef));

            crate::vprintln!("[CEF]    Command line switches applied");
        }
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(TidalBrowserProcessHandler::new(RefCell::new(None)))
        }
        fn render_process_handler(&self) -> Option<RenderProcessHandler> {
            Some(TidalRenderProcessHandler::new(self.renderer_router.clone()))
        }
    }
}

wrap_browser_process_handler! {
    struct TidalBrowserProcessHandler {
        client: RefCell<Option<Client>>,
    }
    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            if let Some(ctx) = cef::request_context_get_global_context() {
                let prefs = [
                    "credentials_enable_service",
                    "profile.password_manager_enabled",
                    "autofill.profile_enabled",
                    "autofill.credit_card_enabled",
                ];
                for pref in prefs {
                    if let Some(mut val) = cef::value_create() {
                        val.set_bool(0);
                        let name = CefString::from(pref);
                        let mut err = CefString::from("");
                        ctx.set_preference(Some(&name), Some(&mut val), Some(&mut err));
                    }
                }
            }

            let data_dir = crate::state::cache_data_dir();
            let pkce_credentials = crate::platform::auth::load_or_create_pkce_credentials(&data_dir);
            let pkce_credentials_json =
                serde_json::to_string(&pkce_credentials).unwrap_or_else(|e| {
                    eprintln!("[PKCE]   Failed to encode credentials for JS: {e}");
                    "{\"credentialsStorageKey\":\"tidal\",\"codeChallenge\":\"\",\"redirectUri\":\"tidal://auth/\",\"codeVerifier\":\"\"}".to_string()
                });

            let platform = if cfg!(target_os = "linux") {
                "linux"
            } else if cfg!(target_os = "macos") {
                "darwin"
            } else {
                "win32"
            };

            let init_script = format!(
                r#"window.__TIDAL_RS_PLATFORM__ = '{platform}';
window.__TIDAL_RS_WINDOW_STATE__ = {{
    isMaximized: false,
    isFullscreen: false
}};
window.__TIDAL_RS_CREDENTIALS__ = {pkce_credentials_json};
var _cfgTarget = {{ enableDesktopFeatures: true }};
var _cfgProxy = new Proxy(_cfgTarget, {{
    get: function(t, p) {{ return p === 'enableDesktopFeatures' ? true : t[p]; }},
    set: function(t, p, v) {{ if (p !== 'enableDesktopFeatures') t[p] = v; return true; }}
}});
Object.defineProperty(window, 'TIDAL_CONFIG', {{
    get: function() {{ return _cfgProxy; }},
    set: function(v) {{
        var src = (v && typeof v === 'object') ? v : {{}};
        var keys = Object.keys(src);
        for (var i = 0; i < keys.length; i++) {{
            if (keys[i] !== 'enableDesktopFeatures') _cfgTarget[keys[i]] = src[keys[i]];
        }}
    }},
    configurable: true
}});
document.title = "TidaLunar - A TIDAL client";
(function() {{
    var css = '[class*="_bar_"] > [class*="_title_"] {{ font-size:0 !important; }} [class*="_bar_"] > [class*="_title_"]::after {{ content:"TidaLunar - A TIDAL client"; font-size:0.75rem; }} header, [role="banner"], nav[class*="bar"] {{ -webkit-app-region: drag; }} header a, header button, header input, header [role="button"], header img, header svg, [role="banner"] a, [role="banner"] button, [role="banner"] input, [role="banner"] [role="button"], nav[class*="bar"] a, nav[class*="bar"] button, nav[class*="bar"] input, nav[class*="bar"] [role="button"] {{ -webkit-app-region: no-drag; }}';
    function inject() {{
        if (document.getElementById('tidalunar-branding')) return;
        var s = document.createElement('style');
        s.id = 'tidalunar-branding';
        s.textContent = css;
        document.head.prepend(s);
    }}
    if (document.head) {{
        inject();
    }} else {{
        document.addEventListener('DOMContentLoaded', inject);
    }}
}})();"#,
                platform = platform,
                pkce_credentials_json = pkce_credentials_json
            );

            let bundle_script = include_str!(concat!(env!("OUT_DIR"), "/bundle.js")).to_string();

            let config = MessageRouterConfig::default();
            let browser_router = BrowserSideRouter::new(config);
            browser_router.add_handler(Arc::new(IpcQueryHandler), false);

            let life_span = TidalLifeSpanHandler::new(browser_router.clone());
            let load = TidalLoadHandler::new(init_script, bundle_script, Cell::new(PageState::Initial));
            let request = TidalRequestHandler::new(browser_router.clone());
            let display = TidalDisplayHandler::new(0);
            let drag = TidalDragHandler::new(0);
            let download = TidalDownloadHandler::new(0);
            let permission = TidalPermissionHandler::new(0);

            {
                let mut client = self.client.borrow_mut();
                *client = Some(TidalClient::new(
                    life_span,
                    load,
                    request,
                    display,
                    drag,
                    download,
                    permission,
                    browser_router,
                ));
            }

            let settings = BrowserSettings {
                background_color: 0xFF111111,
                ..Default::default()
            };
            let url = CefString::from("https://desktop.tidal.com/");

            let mut client_ref = self.default_client();
            let mut bv_delegate = TidalBrowserViewDelegate::new(0);
            let browser_view = browser_view_create(
                client_ref.as_mut(),
                Some(&url),
                Some(&settings),
                None,
                None,
                Some(&mut bv_delegate),
            );

            let mut window_delegate = TidalWindowDelegate::new(RefCell::new(browser_view));
            window_create_top_level(Some(&mut window_delegate));
            crate::vprintln!("[CEF]    Initialized");
        }

        fn default_client(&self) -> Option<Client> {
            self.client.borrow().clone()
        }
    }
}

fn schedule_window_save() {
    let mut task = WindowSaveTask::new(0);
    post_delayed_task(ThreadId::UI, Some(&mut task), 500);
}

wrap_task! {
    struct WindowSaveTask {
        _p: u8,
    }
    impl Task {
        fn execute(&self) {
            with_state(|state| {
                state.window_save_scheduled = false;
                if let Some(ws) = state.pending_window_save.take() {
                    if ws.maximized {
                        state.app_settings.save_maximized(true);
                    } else {
                        state.app_settings.save_window_state(&ws);
                    }
                }
            });
        }
    }
}
