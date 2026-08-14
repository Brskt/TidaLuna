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
            let is_popup = browser.as_ref().map(|b| b.is_popup() != 0).unwrap_or(false);
            let frame_for_inject = frame.as_ref().map(|f| (*f).clone());
            self.router
                .on_context_created(browser.cloned(), frame.cloned(), context.cloned());
            if let Some(ref frame) = frame_for_inject {
                let url = frame.url();
                let url_str = crate::ui::token_filter::userfree_to_string(&url);
                use crate::ui::nav::{self, NavigationPolicy, PageKind};
                if NavigationPolicy::for_page(PageKind::classify(&url_str)).inject_early_runtime {
                    // Skip the fallback bar in auth popups: its window.* buttons hit the
                    // main window (AppWindow::current), and the popup is OS-framed anyway.
                    let titlebar = if is_popup {
                        ""
                    } else {
                        include_str!("early_runtime/fallback_titlebar.js")
                    };
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
                        {host_modules}\
                        {ipc}\
                        {token}\
                        {fetch}\
                        {open}\
                        {session}\
                        {exfil}\
                        {titlebar}\
                        self.__LUNAR_EARLY_RUNTIME__=true;\
                        }})();",
                        desktop = nav::HOST_DESKTOP,
                        login = nav::HOST_LOGIN,
                        auth = nav::HOST_AUTH,
                        api = nav::HOST_API,
                        redirect = nav::REDIRECT_URI,
                        host_modules = include_str!("early_runtime/host_modules.js"),
                        ipc = include_str!("early_runtime/ipc.js"),
                        token = include_str!("early_runtime/token_capture.js"),
                        fetch = include_str!("early_runtime/fetch_proxy.js"),
                        open = include_str!("early_runtime/window_open.js"),
                        session = include_str!("early_runtime/session_stub.js"),
                        exfil = include_str!("early_runtime/exfil_guard.js"),
                        titlebar = titlebar,
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
    "ImmersiveReadAnything",
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
                .map(|s| s.to_string())
                .unwrap_or_default();
            if !proc_type.is_empty() {
                return;
            }

            let Some(cmd) = command_line else { return };

            for s in CEF_SWITCHES {
                let name = CefString::from(*s);
                cmd.append_switch(Some(&name));
            }

            let disable = CEF_DISABLED_FEATURES.join(",");
            let disable_name = CefString::from("disable-features");
            let disable_val = CefString::from(disable.as_str());
            cmd.append_switch_with_value(Some(&disable_name), Some(&disable_val));

            let enable = CEF_ENABLED_FEATURES.join(",");
            let enable_name = CefString::from("enable-features");
            let enable_val = CefString::from(enable.as_str());
            cmd.append_switch_with_value(Some(&enable_name), Some(&enable_val));

            // Linux: force Chromium's "basic" password store. Otherwise OSCrypt
            // reaches for the Secret Service (gnome-libsecret/kwallet) to protect
            // its cookie-encryption key, which pops a keyring-unlock dialog on
            // first launch. The TIDAL token is kept out of the CEF store and
            // persisted separately; at-rest cookie encryption isn't relied on.
            // Must use the name/value form: append_switch("password-store=basic")
            // stores the whole string as the switch key, and OSCrypt's in-process
            // GetSwitchValueASCII("password-store") lookup then misses it.
            #[cfg(target_os = "linux")]
            {
                let name = CefString::from("password-store");
                let value = CefString::from("basic");
                cmd.append_switch_with_value(Some(&name), Some(&value));
            }

            // Linux: force the X11 ozone backend (XWayland under Wayland): the
            // WM decorates the window, which works on GNOME Mutter where
            // Wayland server-side decorations do not.
            #[cfg(target_os = "linux")]
            {
                let name = CefString::from("ozone-platform");
                if cmd.has_switch(Some(&name)) != 1 {
                    let value = CefString::from("x11");
                    cmd.append_switch_with_value(Some(&name), Some(&value));
                }
            }

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
            // CEF is up: the single-instance focus listener may now post UI tasks.
            crate::platform::app_lock::mark_context_ready();

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
                    crate::verr!("[PKCE]   Failed to encode credentials for JS: {e}");
                    format!("{{\"credentialsStorageKey\":\"tidal\",\"codeChallenge\":\"\",\"redirectUri\":\"{}\",\"codeVerifier\":\"\"}}", crate::ui::nav::REDIRECT_URI)
                });

            let platform = if cfg!(target_os = "linux") {
                "linux"
            } else if cfg!(target_os = "macos") {
                "darwin"
            } else {
                "win32"
            };

            let mut close_to_tray = crate::state::boot_settings().close_to_tray;
            if close_to_tray && !crate::platform::tray::create_tray() {
                close_to_tray = false;
            }
            if close_to_tray {
                crate::app_state::with_state(|state| {
                    state.close_to_tray = true;
                });
            }
            crate::platform::tray::start_event_polling();

            let auto_check = crate::state::boot_settings().auto_check;

            let update_channel = if crate::state::boot_settings().update_dev_channel {
                "dev"
            } else {
                "stable"
            };

            let receiver_always_on = crate::state::boot_settings().receiver_always_on;

            let volume_sync = crate::state::boot_settings().volume_sync;

            let asio = crate::state::boot_settings().asio;

            let exclusive = crate::state::boot_settings().exclusive;

            // TIDAL's web frontend gates its native titlebar component on a
            // Windows platform token in navigator.userAgent. On Linux we keep the
            // real Linux UA for network traffic (CefSettings.user_agent) but
            // expose a Windows-like UA to JavaScript for the titlebar to render.
            #[cfg(target_os = "linux")]
            let ua_override = format!(
                r#"(function(){{try{{var UA={ua:?};var AV=UA.replace('Mozilla/','');Object.defineProperty(Navigator.prototype,'userAgent',{{get:function(){{return UA;}},configurable:true}});Object.defineProperty(navigator,'userAgent',{{get:function(){{return UA;}},configurable:true}});Object.defineProperty(Navigator.prototype,'appVersion',{{get:function(){{return AV;}},configurable:true}});Object.defineProperty(navigator,'appVersion',{{get:function(){{return AV;}},configurable:true}});}}catch(e){{}}}})();"#,
                ua = crate::state::JS_USER_AGENT.as_str(),
            );
            #[cfg(not(target_os = "linux"))]
            let ua_override = String::new();

            let perf = crate::debug::perf_monitor::enabled();
            let window_maximized = crate::state::boot_settings().window_maximized;
            let console = crate::state::boot_settings().console;

            // Join the boot-token reconcile: the restored session must be in
            // AppState before the page can consume it. An unusable blob arms a
            // one-shot renderer purge (NEEDS_BOOT_BLOB_PURGE), not an init-script
            // prefix that would replay on every reload.
            crate::finish_boot_tokens();

            let init_script = format!(
                r#"{ua_override}window.__TIDALUNAR_PLATFORM__ = '{platform}';
window.__TIDALUNAR_CLOSE_TO_TRAY__ = {close_to_tray};
window.__TIDALUNAR_AUTO_CHECK__ = {auto_check};
window.__TIDALUNAR_UPDATE_CHANNEL__ = '{update_channel}';
window.__TIDALUNAR_RECEIVER_ALWAYS_ON__ = {receiver_always_on};
window.__TIDALUNAR_VOLUME_SYNC__ = {volume_sync};
window.__TIDALUNAR_ASIO__ = {asio};
window.__TIDALUNAR_EXCLUSIVE__ = {exclusive};
window.__TIDALUNAR_CONSOLE__ = {console};
window.__TIDALUNAR_PERF__ = {perf};
window.__TIDALUNAR_WINDOW_STATE__ = {{
    isMaximized: {window_maximized},
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
}})();
(function() {{
    // Drop the SW + caches for the shell/chunks to re-precache. Both self-heals below
    // self-stop once the cached resources are the rewritten ones.
    function bust() {{
        try {{
            if (!('serviceWorker' in navigator) || !navigator.serviceWorker.controller || !window.caches) return;
            navigator.serviceWorker.getRegistrations().then(function(rs) {{ rs.forEach(function(r) {{ r.unregister(); }}); }});
            caches.keys().then(function(ks) {{ ks.forEach(function(k) {{ caches.delete(k); }}); }});
        }} catch (e) {{}}
    }}
    // 1) Stale CSP meta on a SW-served shell -> re-precache the stripped shell.
    function cspHeal() {{
        if (document.querySelector('meta[http-equiv="Content-Security-Policy" i]')) bust();
    }}
    // 2) A cache predating module-capture serves un-rewritten React chunks, and the
    //    host React never registers. Bust once per session (sessionStorage guard
    //    prevents a loop if the chunk pattern ever stops matching) to re-precache.
    function reactHeal() {{
        try {{
            if (sessionStorage.getItem('__luna_react_heal')) return;
            if (!navigator.serviceWorker || !navigator.serviceWorker.controller) return;
            if (window.__lunaHostModules && window.__lunaHostModules.react) return;
            sessionStorage.setItem('__luna_react_heal', '1');
            bust();
        }} catch (e) {{}}
    }}
    function run() {{ cspHeal(); setTimeout(reactHeal, 5000); }}
    if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', run);
    else run();
}})();"#,
                ua_override = ua_override,
                platform = platform,
                close_to_tray = close_to_tray,
                pkce_credentials_json = pkce_credentials_json
            );

            // Precompose the guard-wrapped injection once (this runs a single time at boot); the
            // nav path then borrows it, instead of re-format!-copying the whole bundle per load.
            let bundle_script = format!(
                "if(!window.__TL_INJECTED__){{window.__TL_INJECTED__=true;{}}}",
                include_str!(concat!(env!("OUT_DIR"), "/bundle.js"))
            );

            let config = MessageRouterConfig::default();
            let browser_router = BrowserSideRouter::new(config);
            browser_router.add_handler(Arc::new(IpcQueryHandler::default()), false);

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
            // Shared context keeps the global profile (cookies/auth) while attaching
            // the handler that strips CSP from the SW's browser-less shell precache.
            let mut doc_ctx_handler = crate::ui::csp_filter::DocumentContextHandler::new();
            let mut request_context = cef::request_context_get_global_context()
                .and_then(|mut global| {
                    cef::request_context_cef_create_context_shared(
                        Some(&mut global),
                        Some(&mut doc_ctx_handler),
                    )
                });
            let browser_view = browser_view_create(
                client_ref.as_mut(),
                Some(&url),
                Some(&settings),
                None,
                request_context.as_mut(),
                Some(&mut bv_delegate),
            );

            let mut window_delegate = TidalWindowDelegate::new(RefCell::new(browser_view), OnceCell::new());
            window_create_top_level(Some(&mut window_delegate));
            crate::vprintln!("[CEF]    Initialized");
        }

        fn default_client(&self) -> Option<Client> {
            self.client.borrow().clone()
        }

        // A duplicate launch handed to us by the process singleton: focus the
        // existing window and return 1 to suppress the default blank relaunch window.
        fn on_already_running_app_relaunch(
            &self,
            _command_line: Option<&mut CommandLine>,
            _current_directory: Option<&CefString>,
        ) -> i32 {
            if let Some(window) = crate::ui::app_window::AppWindow::current() {
                window.restore();
                window.show();
                window.focus_foreground();
            }
            1
        }
    }
}
