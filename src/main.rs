#![cfg_attr(
    all(not(debug_assertions), not(feature = "console")),
    windows_subsystem = "windows"
)]
mod auth;
mod bandwidth;
mod bridge;
mod decrypt;
mod flac_meta;
mod logging;
mod metadata;
mod player;
mod preload;
mod settings;
mod state;

use auth::load_or_create_pkce_credentials;
use bridge::PlayerBridgeEvent;
use cef::wrapper::message_router::{
    BrowserSideHandler, BrowserSideRouter, MessageRouterBrowserSide,
    MessageRouterBrowserSideHandlerCallbacks, MessageRouterConfig, MessageRouterRendererSide,
    MessageRouterRendererSideHandlerCallbacks, RendererSideRouter,
};
use cef::*;
use metadata::parse_track_metadata;
use player::ipc::{PlayerIpc, parse_player_ipc};
use player::{Player, PlayerEvent};
use serde::Deserialize;
use state::TrackInfo;
use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex};

#[derive(Deserialize, Debug)]
struct IpcMessage {
    channel: String,
    #[serde(default)]
    args: Vec<serde_json::Value>,
    #[serde(default)]
    id: Option<String>,
}

// ---------------------------------------------------------------------------
// Shared application state – accessible from CEF callbacks and posted tasks
// ---------------------------------------------------------------------------

struct AppState {
    player: Arc<Player>,
    rt_handle: tokio::runtime::Handle,
    pending_time_update: Option<(f64, u32)>,
    pending_player_events: Vec<PlayerBridgeEvent>,
    pending_misc_js: Vec<String>,
    #[allow(dead_code)] // used when window state save is wired up
    app_settings: settings::Settings,
    // The main browser reference – set once on_after_created fires
    browser: Option<Browser>,
    /// Prevents scheduling duplicate delayed flush tasks.
    flush_scheduled: bool,
}

// SAFETY: AppState is only accessed on the CEF UI thread (single-threaded access)
// except for `player` and `rt_handle` which are Send+Sync.
// The Arc<Mutex<>> is needed for the borrow checker at callback boundaries,
// but actual concurrent access does not occur.
unsafe impl Send for AppState {}
unsafe impl Sync for AppState {}

static APP_STATE: std::sync::OnceLock<Arc<Mutex<AppState>>> = std::sync::OnceLock::new();

fn with_state<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut AppState) -> R,
{
    APP_STATE.get().map(|s| {
        let mut guard = s.lock().unwrap();
        f(&mut guard)
    })
}

/// Execute a JavaScript snippet on a given CEF frame.
fn exec_js_on_frame(frame: &Frame, js: &str) {
    let code = CefString::from(js);
    let url = CefString::from("");
    frame.execute_java_script(Some(&code), Some(&url), 0);
}

/// Execute JavaScript in the main browser frame.
#[allow(dead_code)] // will be used for direct JS evaluation
fn eval_js(js: &str) {
    with_state(|state| {
        if let Some(ref browser) = state.browser
            && let Some(frame) = browser.main_frame()
        {
            exec_js_on_frame(&frame, js);
        }
    });
}

// ---------------------------------------------------------------------------
// IPC handler – processes messages from the frontend
// ---------------------------------------------------------------------------

fn handle_ipc_message(request: &str) {
    let msg: IpcMessage = match serde_json::from_str(request) {
        Ok(m) => m,
        Err(_) => {
            crate::vprintln!("Received unknown IPC message: {}", request);
            return;
        }
    };

    if msg.channel == "player.dbg" {
        crate::vprintln!("[JS-DBG] {:?}", msg.args);
        return;
    }

    crate::vprintln!("IPC Message: {:?}", msg);

    if msg.channel.starts_with("player.") {
        handle_player_ipc(&msg);
    } else {
        handle_window_ipc(&msg);
    }
}

fn handle_player_ipc(msg: &IpcMessage) {
    with_state(|state| {
        match parse_player_ipc(&msg.channel, &msg.args, msg.id.as_deref()) {
            Ok(player_ipc) => match player_ipc {
                PlayerIpc::Load { url, format, key } => {
                    if let Err(e) = state.player.load(url, format, key) {
                        eprintln!("Failed to load track: {}", e);
                    }
                }
                PlayerIpc::Recover {
                    url,
                    format,
                    key,
                    target_time,
                } => {
                    if let Err(e) = state.player.recover(url, format, key, target_time) {
                        eprintln!("Failed to recover track: {}", e);
                    }
                }
                PlayerIpc::Preload { url, key } => {
                    let track = TrackInfo { url, key };
                    state.rt_handle.spawn(async move {
                        preload::start_preload(track).await;
                    });
                }
                PlayerIpc::PreloadCancel => {
                    state.rt_handle.spawn(async {
                        preload::cancel_preload().await;
                    });
                }
                PlayerIpc::Metadata { payload } => {
                    let meta = parse_track_metadata(&payload);
                    let mut lock = crate::state::CURRENT_METADATA.lock().unwrap();
                    *lock = Some(meta);
                }
                PlayerIpc::Play => {
                    let _ = state.player.play();
                }
                PlayerIpc::Pause => {
                    let _ = state.player.pause();
                }
                PlayerIpc::Stop => {
                    let _ = state.player.stop();
                }
                PlayerIpc::Seek { time } => {
                    let seq = crate::player::LOAD_SEQ.load(std::sync::atomic::Ordering::Relaxed);
                    state.pending_time_update = Some((time, seq));
                    // Flush immediately for seek responsiveness
                    flush_bridge_now(state);
                    let _ = state.player.seek(time);
                }
                PlayerIpc::Volume { volume } => {
                    let _ = state.player.set_volume(volume);
                }
                PlayerIpc::DevicesGet { request_id } => {
                    let _ = state.player.get_audio_devices(request_id);
                }
                PlayerIpc::DevicesSet { id, exclusive } => {
                    let _ = state.player.set_audio_device(id, exclusive);
                }
            },
            Err(e) => {
                crate::vprintln!("[IPC]    Invalid player IPC ({}): {:?}", msg.channel, e);
            }
        }
    });
}

fn handle_window_ipc(msg: &IpcMessage) {
    match msg.channel.as_str() {
        "window.close" => {
            with_state(|state| {
                if let Some(window) = get_cef_window(state) {
                    window.close();
                } else {
                    quit_message_loop();
                }
            });
        }
        "window.minimize" => {
            with_state(|state| {
                if let Some(window) = get_cef_window(state) {
                    // On Windows, avoid CEF's window.minimize() which uses
                    // synchronous SendMessage internally, causing reentrancy
                    // deadlock in HWNDMessageHandler. PostMessageW queues the
                    // message so it's processed as a fresh top-level dispatch.
                    #[cfg(target_os = "windows")]
                    {
                        use windows_sys::Win32::UI::WindowsAndMessaging::*;
                        let hwnd = window.window_handle().0 as windows_sys::Win32::Foundation::HWND;
                        if !hwnd.is_null() {
                            unsafe {
                                PostMessageW(hwnd, WM_SYSCOMMAND, SC_MINIMIZE as usize, 0);
                            }
                        }
                    }
                    #[cfg(not(target_os = "windows"))]
                    window.minimize();
                }
            });
        }
        // Both maximize and unmaximize toggle based on the actual window state
        // from CEF, so the JS-side isMaximized() doesn't need to be in sync.
        "window.maximize" | "window.unmaximize" => {
            with_state(|state| {
                if let Some(window) = get_cef_window(state) {
                    let maximized = window.is_maximized() == 1;
                    #[cfg(target_os = "windows")]
                    {
                        use windows_sys::Win32::UI::WindowsAndMessaging::*;
                        let hwnd = window.window_handle().0 as windows_sys::Win32::Foundation::HWND;
                        if !hwnd.is_null() {
                            let cmd = if maximized { SC_RESTORE } else { SC_MAXIMIZE };
                            unsafe {
                                PostMessageW(hwnd, WM_SYSCOMMAND, cmd as usize, 0);
                            }
                        }
                    }
                    #[cfg(not(target_os = "windows"))]
                    {
                        if maximized {
                            window.restore();
                        } else {
                            window.maximize();
                        }
                    }
                    notify_window_state(state, !maximized, false);
                }
            });
        }
        "window.drag" => {
            // Drag is handled by CSS -webkit-app-region: drag
            // + DragHandler forwarding to Window::set_draggable_regions.
        }
        "web.loaded" => {}
        _ => {}
    }
}

/// Notify the frontend of window state changes so the maximize/restore
/// button icon stays in sync.
fn notify_window_state(state: &mut AppState, maximized: bool, fullscreen: bool) {
    let js = format!(
        "if (window.__TIDAL_CALLBACKS__ && window.__TIDAL_CALLBACKS__.window) \
         {{ window.__TIDAL_CALLBACKS__.window.updateState({maximized}, {fullscreen}); }}"
    );
    state.pending_misc_js.push(js);
    flush_bridge_now(state);
}

/// Get the CEF Window from the current browser.
fn get_cef_window(state: &AppState) -> Option<Window> {
    let mut browser = state.browser.clone();
    let bv = browser_view_get_for_browser(browser.as_mut())?;
    bv.window()
}

/// Flush pending bridge events to the browser immediately.
fn flush_bridge_now(state: &mut AppState) {
    if let Some((time, seq)) = state.pending_time_update.take() {
        state
            .pending_player_events
            .push(PlayerBridgeEvent::time(time, seq));
    }

    if !state.pending_player_events.is_empty() {
        if let Ok(events_json) = serde_json::to_string(&state.pending_player_events) {
            let js = format!(
                "if (window.__TIDAL_RS_PLAYER_PUSH__) {{ window.__TIDAL_RS_PLAYER_PUSH__({}); }}",
                events_json
            );
            if let Some(ref browser) = state.browser
                && let Some(frame) = browser.main_frame()
            {
                exec_js_on_frame(&frame, &js);
            }
        }
        state.pending_player_events.clear();
    }

    if !state.pending_misc_js.is_empty() {
        let js_batch = state.pending_misc_js.join(";");
        if let Some(ref browser) = state.browser
            && let Some(frame) = browser.main_frame()
        {
            exec_js_on_frame(&frame, &js_batch);
        }
        state.pending_misc_js.clear();
    }
}

// ---------------------------------------------------------------------------
// Player event handler – called from the player thread, posts to CEF UI thread
// ---------------------------------------------------------------------------

fn handle_player_event(event: PlayerEvent) {
    with_state(|state| {
        let mut should_flush = true;
        match event {
            PlayerEvent::TimeUpdate(time, seq) => {
                state.pending_time_update = Some((time, seq));
                if time != 0.0 {
                    should_flush = false; // batch time updates
                }
            }
            PlayerEvent::StateChange(st, seq) => {
                crate::vprintln!("[BRIDGE] StateChange: \"{}\" seq={}", st, seq);
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::state(st, seq));
            }
            PlayerEvent::Duration(duration, seq) => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::duration(duration, seq));
            }
            PlayerEvent::AudioDevices(devices, req_id) => {
                if let Ok(json_devices) = serde_json::to_string(&devices) {
                    if let Some(id) = req_id {
                        let js = format!(
                            "window.__TIDAL_IPC_RESPONSE__('{}', null, {})",
                            id, json_devices
                        );
                        state.pending_misc_js.push(js);
                    } else {
                        state
                            .pending_player_events
                            .push(PlayerBridgeEvent::devices(serde_json::json!(devices)));
                    }
                }
            }
            PlayerEvent::MediaFormat {
                codec,
                sample_rate,
                bit_depth,
                channels,
            } => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::media_format(
                        codec,
                        sample_rate,
                        bit_depth,
                        channels,
                    ));
            }
            PlayerEvent::Version(v) => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::version(v));
            }
            PlayerEvent::DeviceError(event_name) => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::device_error(event_name));
            }
            PlayerEvent::MediaError { error, code } => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::media_error(&error, code));
            }
            PlayerEvent::MaxConnectionsReached => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::max_connections());
            }
        }
        if should_flush {
            flush_bridge_now(state);
        } else if !state.flush_scheduled {
            // Schedule a delayed flush (24ms batching for time updates)
            state.flush_scheduled = true;
            schedule_flush_task();
        }
    });
}

fn schedule_flush_task() {
    let mut task = FlushTask::new(0);
    post_delayed_task(ThreadId::UI, Some(&mut task), 24);
}

wrap_task! {
    struct FlushTask {
        _p: u8,
    }
    impl Task {
        fn execute(&self) {
            with_state(|state| {
                state.flush_scheduled = false;
                flush_bridge_now(state);
            });
        }
    }
}

// ---------------------------------------------------------------------------
// CEF handlers
// ---------------------------------------------------------------------------

// --- IPC Query Handler (JS → Rust via cefQuery) ---

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
        handle_ipc_message(request);
        // Respond with success (fire-and-forget).
        // Use "ok" instead of "" to avoid "Invalid UTF-16 string" warnings
        // from empty CefString conversion in the response path.
        callback.lock().unwrap().success_str("ok");
        true
    }
}

// --- Drag Handler (forward drag regions to Window for frameless drag) ---

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

// --- Download Handler (block all downloads) ---

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
            0 // deny all file downloads
        }
    }
}

// --- Permission Handler (deny all permissions) ---

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
            1 // handled — denied
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
            1 // handled — denied
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
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            if let Some(browser) = browser.cloned() {
                with_state(|state| {
                    state.browser = Some(browser);
                });
            }
        }
        fn do_close(&self, _browser: Option<&mut Browser>) -> i32 {
            0 // allow close
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

// --- Load Handler (inject initialization scripts) ---

wrap_load_handler! {
    struct TidalLoadHandler {
        init_script: String,
        bundle_script: String,
        injected: Cell<bool>,
    }
    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            browser: Option<&mut Browser>,
            is_loading: i32,
            _can_go_back: i32,
            _can_go_forward: i32,
        ) {
            // Inject scripts only on the first page load.  Re-injecting on
            // SPA navigations would recreate NativePlayerComponent from
            // scratch, destroying the SDK's emitter and all its listeners.
            if is_loading == 0
                && !self.injected.get()
                && let Some(browser) = browser
                && let Some(frame) = browser.main_frame()
            {
                self.injected.set(true);
                exec_js_on_frame(&frame, &self.init_script);
                exec_js_on_frame(&frame, &self.bundle_script);
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
            // Intercept tidal:// custom scheme (OAuth redirect)
            if let Some(req) = request {
                let url_userfree = req.url();
                let url_cef: CefString = CefString::from(&url_userfree);
                let url = format!("{}", url_cef);
                if url.starts_with("tidal://") {
                    // Rewrite tidal://login/auth?code=... → https://desktop.tidal.com/login/auth?code=...
                    let web_url = url.replacen("tidal://", "https://desktop.tidal.com/", 1);
                    crate::vprintln!("[AUTH]   Intercepted tidal:// redirect → {}", web_url);
                    if let Some(frame) = frame {
                        let cef_url = CefString::from(web_url.as_str());
                        frame.load_url(Some(&cef_url));
                    }
                    return 1; // block the tidal:// navigation
                }
            }
            self.router
                .on_before_browse(browser.cloned(), frame.cloned());
            0 // allow navigation
        }
        fn on_certificate_error(
            &self,
            _browser: Option<&mut Browser>,
            _cert_error: Errorcode,
            _request_url: Option<&CefString>,
            _ssl_info: Option<&mut Sslinfo>,
            _callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            0 // reject — never bypass certificate errors
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
                crate::vprintln!("[JS] {msg}");
            }
            0
        }
    }
}

/// Minimal Win32 subclass for CEF frameless window.
///
/// Handles `WM_NCCALCSIZE` to clamp maximized bounds to the monitor work
/// area, and routes `SC_MINIMIZE`/`SC_MAXIMIZE`/`SC_RESTORE` through
/// `DefWindowProcW` to avoid a reentrancy deadlock in Chromium's
/// `HWNDMessageHandler`. Resize borders are handled by CEF's built-in
/// `CaptionlessFrameView` (enabled via `is_frameless = 1`).
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
                // Let DefWindowProcW do internal bookkeeping (placement,
                // WVR_VALIDRECTS) then set the client rect.
                let params = &*(lparam as *const NCCALCSIZE_PARAMS);
                let proposed = params.rgrc[0];
                DefWindowProcW(hwnd, msg, wparam, lparam);
                let params = &mut *(lparam as *mut NCCALCSIZE_PARAMS);

                if IsZoomed(hwnd) != 0 {
                    // When maximized, Windows expands the window beyond the
                    // screen by the frame thickness. Clamp to the monitor's
                    // work area so the window doesn't overflow.
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
                // Bypass CEF — Chromium's HWNDMessageHandler uses synchronous
                // SendMessage for min/max/restore which deadlocks via
                // reentrancy into its own LockUpdates/ScopedRedrawLock.
                DefWindowProcW(hwnd, msg, wparam, lparam)
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
            // When maximized, return default (empty rect) so CEF centers the
            // window at preferred_size when the user un-maximizes.  The saved
            // bounds may be stale full-screen values from before the fix.
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
            // Return 1 (frameless) so CEF uses CaptionlessFrameView
            // which provides built-in resize border hit-testing (4px).
            // On Windows, on_window_created patches the style to add
            // WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX for
            // proper taskbar minimize/restore behavior.
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

            // Set window icon from embedded PNG (taskbar + Alt+Tab on supported platforms)
            if let Some(mut icon) = image_create() {
                let png_data = include_bytes!("../tidaluna.png");
                icon.add_png(1.0, Some(png_data));
                window.set_window_icon(Some(&mut icon));
                window.set_window_app_icon(Some(&mut icon));
            }

            // On Windows, CEF creates a WS_POPUP window for frameless mode.
            // Patch the style to add thick frame + min/max buttons so the
            // taskbar icon works (minimize/restore via click) and the system
            // treats this as a proper resizable app window. Then install a
            // minimal subclass for WM_NCCALCSIZE (maximize work-area clamp)
            // and WM_SYSCOMMAND (deadlock bypass for min/max/restore).
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

            window.show();
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
            // Don't save position while minimized (bounds are invalid).
            if window.is_minimized() == 1 {
                return;
            }
            let maximized = window.is_maximized() == 1;
            with_state(|state| {
                if maximized {
                    // Only save the maximized flag — keep the normal-size
                    // bounds in DB so restore returns to the right size.
                    state.app_settings.save_maximized(true);
                } else {
                    state.app_settings.save_window_state(
                        &crate::settings::WindowState {
                            x: bounds.x,
                            y: bounds.y,
                            width: bounds.width as u32,
                            height: bounds.height as u32,
                            maximized: false,
                        },
                    );
                }
                notify_window_state(state, maximized, false);
            });
        }
        fn can_close(&self, _window: Option<&mut Window>) -> i32 {
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

// --- Render Process Handler (renderer side of message router) ---

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

wrap_app! {
    struct TidalApp {
        renderer_router: Arc<RendererSideRouter>,
    }
    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            // Only apply switches to the browser process — CEF propagates
            // relevant ones to subprocesses automatically.
            let proc_type = _process_type
                .map(|s| format!("{}", s))
                .unwrap_or_default();
            if !proc_type.is_empty() {
                return;
            }

            let Some(cmd) = command_line else { return };

            // --- Switches: disable unnecessary subsystems ---
            // TIDAL needs: DOM, CSS, JS, fetch/XHR, cookies, localStorage,
            //              Canvas 2D, WebGL (cover art/transitions).
            // Everything else is dead weight for a music streaming SPA.
            let switches = [
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
                "no-first-run",
                "no-default-browser-check",
                "disable-hang-monitor",
                "disable-popup-blocking",
                "disable-prompt-on-repost",
                "disable-save-password-bubble",
                "disable-webrtc",
                "site-per-process",
            ];
            for s in &switches {
                let name = CefString::from(*s);
                cmd.append_switch(Some(&name));
            }

            // --- disable-features via append_switch (workaround for
            //     append_switch_with_value crash on Windows) ---
            let features = [
                // Passwords & autofill — all credential/form-fill prompts
                "PasswordManager",
                "PasswordManagerOnboarding",
                "AutofillServerCommunication",
                "AutofillCreditCardEnabled",
                "AutofillProfileEnabled",
                "KeyboardAccessory",
                // Translation
                "Translate",
                "TranslateUI",
                // Payments — server-side only for TIDAL
                "WebPayments",
                "PaymentHandler",
                "SecurePaymentConfirmation",
                "DigitalGoodsAPI",
                // Hardware access — no peripherals needed
                "WebUSB",
                "WebBluetooth",
                "WebHID",
                "WebNFC",
                "WebMidi",
                "Serial",
                "Gamepad",
                // Sensors — no device sensors needed
                "GenericSensorExtraClasses",
                "AmbientLightSensor",
                // Background services — SPA, no offline/push needed
                "BackgroundFetch",
                "BackgroundSync",
                "PushMessaging",
                "IdleDetection",
                // Media capture — no camera/mic/screen capture
                "GetDisplayMedia",
                "MediaSession",
                // Speech — no voice input/output
                "SpeechSynthesis",
                "SpeechRecognition",
                // WebRTC — no video/voice calls
                "WebRtcHideLocalIpsWithMdns",
                // XR
                "WebXR",
                // Privacy Sandbox / ads tracking
                "Topics",
                "Fledge",
                "FledgeInterestGroupAPI",
                "AttributionReporting",
                "PrivateAggregation",
                "SharedStorageAPI",
                "PrivacySandboxAdsAPIsOverride",
                // Federated identity — TIDAL uses own OAuth
                "FedCm",
                "FedCmAutoSigninAPI",
                "WebOTP",
                // Telemetry / client hints
                "ClientHints",
                "UserAgentClientHint",
                "MetricsReportingPolicy",
                "ReportingAPI",
                "DeprecationReporting",
                // Safe browsing — not a general browser
                "SafeBrowsing",
                // Misc APIs TIDAL doesn't use
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
            ];
            let switch = format!("disable-features={}", features.join(","));
            let switch_cef = CefString::from(switch.as_str());
            cmd.append_switch(Some(&switch_cef));

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
            // --- Build initialization script ---
            let data_dir = state::cache_data_dir();
            let pkce_credentials = load_or_create_pkce_credentials(&data_dir);
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

            // --- Create browser-side message router ---
            let config = MessageRouterConfig::default();
            let browser_router = BrowserSideRouter::new(config);
            browser_router.add_handler(Arc::new(IpcQueryHandler), false);

            // --- Build CEF client with handlers ---
            let life_span = TidalLifeSpanHandler::new(browser_router.clone());
            let load = TidalLoadHandler::new(init_script, bundle_script, Cell::new(false));
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

            // --- Create browser view and window ---
            let settings = BrowserSettings {
                background_color: 0xFF111111, // match TIDAL dark theme
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

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let args = cef::args::Args::new();
    let Some(cmd_line) = args.as_cmd_line() else {
        return Err("Failed to parse command line arguments".into());
    };

    let switch = CefString::from("type");
    let is_browser = cmd_line.has_switch(Some(&switch)) != 1;

    // Create renderer-side router (needed by both browser and renderer processes)
    let renderer_config = MessageRouterConfig::default();
    let renderer_router = RendererSideRouter::new(renderer_config);

    // Handle subprocess execution (renderer, GPU, etc.)
    // Pass the App so renderer subprocesses get our RenderProcessHandler
    let mut app = TidalApp::new(renderer_router);
    let ret = execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    if !is_browser {
        std::process::exit(ret);
    }

    // --- Initialize tokio runtime ---
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let rt_handle = rt.handle().clone();

    // Force-initialize statics that require tokio
    {
        let _guard = rt.enter();
        let _ = &*crate::state::GOVERNOR;
        let _ = &*crate::state::AUDIO_CACHE;
    }

    // --- Initialize player ---
    let player = Arc::new(
        Player::new(
            move |event| {
                // Post player event processing to CEF UI thread
                let mut task = PlayerEventTask::new(event);
                post_task(ThreadId::UI, Some(&mut task));
            },
            rt_handle.clone(),
        )
        .expect("Failed to initialize player"),
    );

    // --- Initialize app settings ---
    let app_settings = settings::Settings::open(&state::cache_data_dir())
        .expect("Failed to open settings database");

    // --- Set up shared state ---
    let _ = APP_STATE.set(Arc::new(Mutex::new(AppState {
        player,
        rt_handle,
        pending_time_update: None,
        pending_player_events: Vec::new(),
        pending_misc_js: Vec::new(),
        app_settings,
        browser: None,
        flush_scheduled: false,
    })));

    // --- Initialize CEF ---
    let root_cache = state::cache_data_dir().join("cef");
    let profile_cache = root_cache.join("Default");
    std::fs::create_dir_all(&profile_cache).ok();

    let root_cache_cef = CefString::from(root_cache.to_string_lossy().as_ref());
    let profile_cache_cef = CefString::from(profile_cache.to_string_lossy().as_ref());

    let user_agent = CefString::from(if cfg!(target_os = "linux") {
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) tidal-hifi/1.12.4-beta Chrome/144.0.7559.96 Electron/40.1.0 Safari/537.36"
    } else {
        "Mozilla/5.0 (Windows NT 10.0; WOW64) AppleWebKit/537.36 (KHTML, like Gecko) TIDAL/1.12.4-beta Chrome/142.0.7444.235 Electron/39.2.7 Safari/537.36"
    });

    let settings = Settings {
        no_sandbox: 0,
        root_cache_path: root_cache_cef,
        cache_path: profile_cache_cef,
        user_agent,
        background_color: 0xFF111111, // TIDAL dark theme — no white/Chromium flash
        ..Default::default()
    };

    assert_eq!(
        initialize(
            Some(args.as_main_args()),
            Some(&settings),
            Some(&mut app),
            std::ptr::null_mut(),
        ),
        1,
        "CEF initialization failed"
    );

    run_message_loop();
    shutdown();
    Ok(())
}

// --- Task to process player events on the UI thread ---

wrap_task! {
    struct PlayerEventTask {
        event: PlayerEvent,
    }
    impl Task {
        fn execute(&self) {
            handle_player_event(self.event.clone());
        }
    }
}
