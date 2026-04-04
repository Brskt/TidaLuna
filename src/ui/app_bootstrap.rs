use super::client::*;
use super::window_delegate::*;
use cef::wrapper::message_router::{
    BrowserSideRouter, MessageRouterBrowserSide, MessageRouterConfig,
    MessageRouterRendererSideHandlerCallbacks, RendererSideRouter,
};
use cef::*;
use std::cell::{Cell, OnceCell, RefCell};
use std::sync::Arc;

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
            let frame_for_inject = frame.as_ref().map(|f| (*f).clone());
            self.router
                .on_context_created(browser.cloned(), frame.cloned(), context.cloned());
            if let Some(ref frame) = frame_for_inject {
                let url = frame.url();
                let url_str = crate::ui::token_filter::userfree_to_string(&url);
                use crate::ui::nav::{self, NavigationPolicy, PageKind};
                if NavigationPolicy::for_page(PageKind::classify(&url_str)).inject_early_runtime {
                    let preload = format!(
                        "(function(){{\
                        if(self.__LUNAR_EARLY_RUNTIME__)return;\
                        if(typeof window.cefQuery!=='function')return;\
                        self.__LUNAR_CONFIG__={{\
                            desktopHost:\"{desktop}\",\
                            loginHost:\"{login}\",\
                            authHost:\"{auth}\",\
                            apiHost:\"{api}\",\
                            redirectUri:\"{redirect}\",\
                            loginCallbackPath:\"/login/auth\",\
                            authHosts:[\"{login}\",\"{auth}\"]\
                        }};\
                        {ipc}\
                        {token}\
                        {fetch}\
                        {open}\
                        {session}\
                        {exfil}\
                        self.__LUNAR_EARLY_RUNTIME__=true;\
                        }})();",
                        desktop = nav::HOST_DESKTOP,
                        login = nav::HOST_LOGIN,
                        auth = nav::HOST_AUTH,
                        api = nav::HOST_API,
                        redirect = nav::REDIRECT_URI,
                        ipc = include_str!("early_runtime/ipc.js"),
                        token = include_str!("early_runtime/token_capture.js"),
                        fetch = include_str!("early_runtime/fetch_proxy.js"),
                        open = include_str!("early_runtime/window_open.js"),
                        session = include_str!("early_runtime/session_stub.js"),
                        exfil = include_str!("early_runtime/exfil_guard.js"),
                    );
                    crate::app_state::exec_js_on_frame(frame, &preload);
                }
            }
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
                    "download_bubble.partial_view_enabled",
                    "download_bubble_enabled",
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
                    format!("{{\"credentialsStorageKey\":\"tidal\",\"codeChallenge\":\"\",\"redirectUri\":\"{}\",\"codeVerifier\":\"\"}}", crate::ui::nav::REDIRECT_URI)
                });

            let platform = if cfg!(target_os = "linux") {
                "linux"
            } else if cfg!(target_os = "macos") {
                "darwin"
            } else {
                "win32"
            };

            let mut close_to_tray = crate::state::db()
                .call_settings(crate::settings::load_close_to_tray);
            if close_to_tray && !crate::platform::tray::create_tray() {
                close_to_tray = false;
            }
            if close_to_tray {
                crate::app_state::with_state(|state| {
                    state.close_to_tray = true;
                });
            }
            crate::platform::tray::start_event_polling();

            let auto_check = crate::state::db()
                .call_settings(crate::settings::load_update_auto_check);

            let init_script = format!(
                r#"window.__TIDALUNAR_PLATFORM__ = '{platform}';
window.__TIDALUNAR_CLOSE_TO_TRAY__ = {close_to_tray};
window.__TIDALUNAR_AUTO_CHECK__ = {auto_check};
window.__TIDALUNAR_WINDOW_STATE__ = {{
    isMaximized: false,
    isFullscreen: false
}};
window.__TIDALUNAR_CREDENTIALS__ = {pkce_credentials_json};
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
                close_to_tray = close_to_tray,
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
            let url = CefString::from(format!("https://{}/", crate::ui::nav::HOST_DESKTOP).as_str());

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

            let mut window_delegate = TidalWindowDelegate::new(RefCell::new(browser_view), OnceCell::new());
            window_create_top_level(Some(&mut window_delegate));
            crate::vprintln!("[CEF]    Initialized");
        }

        fn default_client(&self) -> Option<Client> {
            self.client.borrow().clone()
        }
    }
}
