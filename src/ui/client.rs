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

/// Privileged IPC channels inject the real session token or mutate auth/session
/// state, so they must only be honored from a TIDAL-owned or auth frame. The
/// `cefQuery` function is exposed to every frame in the renderer (including
/// cross-origin subframes), so the trust boundary is enforced here in Rust
/// rather than relying on the soft JS-side wrapper isolation. Any frame that
/// classifies as `External` (a malicious iframe, or the main frame redirected
/// off-origin) is rejected.
fn frame_is_trusted(frame: &Option<Frame>) -> bool {
    let Some(frame) = frame else {
        return false;
    };
    let url = crate::ui::token_filter::userfree_to_string(&frame.url());
    !matches!(PageKind::classify(&url), PageKind::External)
}

/// Channels that mutate auth/session/plugin/updater/settings state, so they must
/// never run from an untrusted ingress. Single source of truth for both the
/// `cefQuery` path and the frame-less `__IPC__` console bridge. Benign page-chrome
/// (`player.*`, `window.*`, `menu.*`, `web.*`) is absent so TIDAL's UI keeps working;
/// `player.parse_dash` is the exception (plugin-IPC, frame-gated), so the bridge drops it.
fn is_privileged_channel(channel: &str) -> bool {
    channel.starts_with("jsrt.")
        || channel.starts_with("connect.")
        || channel.starts_with("updater.")
        || channel.starts_with("settings.")
        || channel.starts_with("plugin.")
        || channel.starts_with("proxy.")
        || channel.starts_with("tidal.")
        || channel.starts_with("__Luna.")
        || channel.starts_with("__LunaNative.")
        || channel == "player.parse_dash"
        || channel == "window.navigate_self"
}

pub(super) struct IpcQueryHandler;

impl BrowserSideHandler for IpcQueryHandler {
    fn on_query_str(
        &self,
        _browser: Option<Browser>,
        frame: Option<Frame>,
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
                || msg.channel.starts_with("updater.")
                || msg.channel.starts_with("connect.")
                || msg.channel == "player.parse_dash")
            && msg.id.is_some()
        {
            if !frame_is_trusted(&frame) {
                callback
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .failure(403, "IPC restricted to TIDAL frames");
                return true;
            }
            if msg.channel.starts_with("connect.") {
                if let Some(ref f) = frame
                    && f.is_main() == 0
                {
                    callback
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .failure(403, "Connect IPC restricted to main frame");
                    return true;
                }
                crate::connect::ipc::handle_connect_invoke(msg, callback);
                return true;
            }
            handle_plugin_ipc(msg, callback);
            return true;
        }

        // Fire-and-forget (`sendIpc`, no `id`) skips the branch above. Privileged
        // channels here still mutate auth/session/plugin/updater/settings state,
        // so they must also be restricted to trusted frames. There is no JS-side
        // callback consumer, so an untrusted call is silently dropped (the query
        // is still consumed, return true). Benign page-chrome channels (player.*,
        // window.* controls, menu.clicked, web.loaded) stay reachable from any
        // frame, as TIDAL needs them.
        if let Ok(msg) = serde_json::from_str::<IpcMessage>(request)
            && is_privileged_channel(&msg.channel)
            && !frame_is_trusted(&frame)
        {
            crate::vprintln!(
                "[IPC]    Dropped privileged fire-and-forget from untrusted frame: {}",
                msg.channel
            );
            callback
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .success_str("ok");
            return true;
        }

        handle_ipc_message(request);
        callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
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
            if let Some(window) = crate::ui::app_window::AppWindow::from_browser(browser) {
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

/// Open an external http(s) URL in the OS browser; other schemes are dropped.
fn open_external_in_os(url: &str) {
    if url.starts_with("http://") || url.starts_with("https://") {
        crate::vprintln!(
            "[NAV]    External -> OS browser: {}",
            crate::util::truncate_str(url, 120)
        );
        open_in_os(url);
    } else {
        crate::vprintln!(
            "[NAV]    Blocked external navigation: {}",
            crate::util::truncate_str(url, 120)
        );
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
            crate::vprintln!("[POPUP]  on_before_popup: {}", crate::util::truncate_str(&url_str, 120));
            if target_url.is_some() {
                let kind = PageKind::classify(&url_str);
                if kind == PageKind::AuthHost {
                    crate::vprintln!("[AUTH]   Opening auth popup: {}", crate::util::truncate_str(&url_str, 120));
                    if let Some(wi) = _window_info {
                        wi.window_name = CefString::from("TidaLunar - Login");
                        wi.bounds = cef::Rect { x: 100, y: 100, width: 500, height: 700 };
                        #[cfg(target_os = "windows")]
                        {
                            wi.style = 0x00CF0000; // WS_OVERLAPPEDWINDOW
                        }
                        wi.runtime_style = RuntimeStyle::ALLOY;
                    }
                    return 0;
                }
                if kind == PageKind::External {
                    open_external_in_os(&url_str);
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

/// One-shot guard for the post-login cold-boot reload (see on_loading_state_change).
/// Reset on session_clear so each login triggers exactly one reload, and a bounce
/// back to the login page can't turn it into a loop.
pub(crate) static POST_LOGIN_RELOADED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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
                    crate::vprintln!("[LOAD]   on_load_start init_script: {}", crate::util::truncate_str(&url, 80));
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
                    crate::vprintln!("[LOAD]   Auth page loaded: {}", crate::util::truncate_str(&url, 100));
                }
                if !policy.inject_bundle {
                    return;
                }

                let is_login =
                    matches!(kind, PageKind::LoginPage | PageKind::LoginCallback);
                let prev = self.page_state.get();

                // Transitioning from login to app. TIDAL registers its player SDK
                // only during the cold bootstrap on the app route; the SPA
                // login->app transition reaches the app without re-running it, so
                // the play saga throws "No active player" (the redux activePlayer
                // slice is rehydrated and is not what the saga checks). Reload the
                // app root once to reproduce the known-good cold launch.
                if prev == PageState::Login && !is_login {
                    with_state(|state| {
                        let _ = state.player.stop();
                        state.pending_player_events.clear();
                        state.pending_time_update = None;
                    });
                    if !crate::ui::client::POST_LOGIN_RELOADED
                        .swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        crate::vprintln!("[LOAD]   Post-login reload to re-register the player");
                        self.page_state.set(PageState::App);
                        let cef_url = CefString::from(format!("https://{}/", nav::HOST_DESKTOP).as_str());
                        frame.load_url(Some(&cef_url));
                        return;
                    }
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

                // After post-login SPA navigation to app, signal JS to re-run init().
                // Plugin loading is handled by init() → invokeIpc("jsrt.load_plugins").
                if !is_login && prev == PageState::Login {
                    crate::app_state::emit_ipc_event("jsrt.post_login_init");
                    // Check for updates after login
                    crate::updater::trigger_update_check();
                }

                // Also check on first app load (not coming from login)
                if !is_login && prev == PageState::Initial {
                    crate::updater::trigger_update_check();
                }
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

fn render_crash_reason(status: TerminationStatus) -> &'static str {
    match status {
        TerminationStatus::PROCESS_OOM => "ran out of memory",
        TerminationStatus::PROCESS_CRASHED => "crashed",
        TerminationStatus::PROCESS_WAS_KILLED => "was killed",
        TerminationStatus::ABNORMAL_TERMINATION => "terminated abnormally",
        TerminationStatus::LAUNCH_FAILED => "failed to launch",
        TerminationStatus::INTEGRITY_FAILURE => "failed an integrity check",
        _ => "stopped unexpectedly",
    }
}

/// Write a render-process crash report under the data dir and return its path.
fn write_render_crash_log(
    status: TerminationStatus,
    reason: &str,
    error_code: i32,
    error_string: &str,
) -> Option<std::path::PathBuf> {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let (year, month, day) = (now.year(), u8::from(now.month()), now.day());
    let (hour, min, sec) = (now.hour(), now.minute(), now.second());
    // Filename can't contain `/` or `:`, so the name uses dashes; the body
    // keeps the readable HH:MM:SS DD/MM/YYYY form.
    let file_stamp = format!("{hour:02}-{min:02}-{sec:02}_{day:02}-{month:02}-{year}");
    let human_stamp = format!("{hour:02}:{min:02}:{sec:02} {day:02}/{month:02}/{year}");
    let dir = crate::state::cache_data_dir().join("crashes");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("render-crash-{file_stamp}.log"));
    let body = format!(
        "TidaLunar render process terminated\nversion: {}\ntime: {human_stamp}\nstatus: {reason} (raw {})\nerror_code: {error_code}\nerror_string: {error_string}\n",
        env!("CARGO_PKG_VERSION"),
        status.get_raw(),
    );
    std::fs::write(&path, body).ok()?;
    Some(path)
}

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
                crate::util::truncate_str(&url, 200)
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
        fn on_open_urlfrom_tab(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            target_url: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            // DevTools link clicks route here, not through on_before_browse; send
            // them to the OS browser instead of navigating the inspected window.
            let url = target_url.as_ref().map(|u| u.to_string()).unwrap_or_default();
            open_external_in_os(&url);
            1
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

            // Strip the CSP meta on the doc navigation (first load, no SW yet);
            // the browser handler wins over the context one, so peel it off here.
            // TokenResourceHandler is a no-op on the doc GET, so nothing is lost.
            if crate::ui::csp_filter::is_document_url(&url) {
                return Some(crate::ui::csp_filter::DocumentHandler::new());
            }

            // Rewrite React-family chunks to capture TIDAL's real React exports
            // so plugins share the host instance (hooks/context).
            if crate::ui::module_capture::target_module_id(&url).is_some() {
                return Some(crate::ui::module_capture::CaptureRequestHandler::new());
            }

            if crate::ui::token_filter::should_rewrite_token(&url)
                || crate::ui::nav::is_token_endpoint(&url)
            {
                Some(crate::ui::token_filter::TokenResourceHandler::new())
            } else if !crate::ui::nav::is_tidal_origin(&url) {
                // Exfiltration guard: block sendBeacon to non-Tidal domains.
                // TIDAL doesn't use sendBeacon - safe to block unconditionally.
                if let Some(req) = _request.as_ref()
                    && req.resource_type() == ResourceType::PING
                {
                    crate::vprintln!(
                        "[EXFIL]  BLOCKED sendBeacon to {}",
                        crate::util::truncate_str(&url, 80)
                    );
                    return Some(crate::ui::token_filter::ExfilBlockHandler::new());
                }
                None
            } else {
                None
            }
        }
        fn on_render_process_terminated(
            &self,
            browser: Option<&mut Browser>,
            status: TerminationStatus,
            error_code: ::std::os::raw::c_int,
            error_string: Option<&CefString>,
        ) {
            let owned = browser.cloned();
            self.router.on_render_process_terminated(owned.clone());

            let reason = render_crash_reason(status);
            let err = error_string.map(|s| s.to_string()).unwrap_or_default();
            crate::vprintln!("[CRASH]  Render process {reason} (code {error_code}) {err}");
            let log_path = write_render_crash_log(status, reason, error_code, &err);

            // Only the main browser triggers the recovery dialog; a crashing
            // auth popup must not prompt a reload of the wrong window.
            let is_main = owned.as_ref().is_some_and(|b| b.is_popup() == 0);
            if !is_main {
                return;
            }

            // Stack guard: if the reloaded page crashes again while a dialog is
            // already up, just reload rather than opening another dialog.
            if crate::ui::crash_dialog::CRASH_DIALOG_OPEN
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                if let Some(b) = owned {
                    b.reload();
                }
                return;
            }

            let rx = crate::ui::crash_dialog::show_crash_dialog(
                reason,
                error_code,
                log_path.as_deref(),
            );
            crate::state::rt_handle().spawn(async move {
                let action = rx
                    .await
                    .unwrap_or(crate::ui::crash_dialog::CrashAction::Reload);
                crate::ui::crash_dialog::CRASH_DIALOG_OPEN
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                if action == crate::ui::crash_dialog::CrashAction::Quit {
                    std::process::exit(0);
                }
                if action == crate::ui::crash_dialog::CrashAction::OpenFolder
                    && let Some(dir) = log_path.as_ref().and_then(|p| p.parent())
                {
                    open_in_os(dir);
                }
                // Reload the main browser on the UI thread (for Reload and
                // OpenFolder; Quit already exited above).
                let mut task = crate::ui::crash_dialog::ReloadMainTask::new(0);
                post_task(ThreadId::UI, Some(&mut task));
            });
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
            if let Some(window) = crate::ui::app_window::AppWindow::from_browser(browser) {
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
                    // The console bridge carries no frame, so origin trust cannot
                    // be established here. Privileged channels are refused on this
                    // path (they must use the origin-gated cefQuery router); only
                    // benign page-chrome channels are dispatched.
                    if let Ok(parsed) = serde_json::from_str::<IpcMessage>(json)
                        && is_privileged_channel(&parsed.channel)
                    {
                        crate::vprintln!(
                            "[IPC]    Dropped privileged __IPC__ console message: {}",
                            parsed.channel
                        );
                        return 0;
                    }
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

#[cfg(test)]
mod tests {
    use super::is_privileged_channel;

    #[test]
    fn parse_dash_is_privileged() {
        // Dispatched by the plugin-IPC handler and frame-gated in cefQuery, so
        // the frame-less console bridge must drop it too.
        assert!(is_privileged_channel("player.parse_dash"));
    }

    #[test]
    fn benign_player_controls_stay_open() {
        // Playback controls are fire-and-forget on the console bridge; only
        // parse_dash is gated, not the whole `player.*` namespace.
        assert!(!is_privileged_channel("player.play"));
        assert!(!is_privileged_channel("player.load_dash"));
    }

    #[test]
    fn auth_and_window_self_are_privileged() {
        assert!(is_privileged_channel("jsrt.set_token"));
        assert!(is_privileged_channel("window.navigate_self"));
        assert!(!is_privileged_channel("window.minimize"));
    }
}
