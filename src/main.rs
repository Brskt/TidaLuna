#![cfg_attr(
    all(not(debug_assertions), not(feature = "console")),
    windows_subsystem = "windows"
)]
mod bandwidth;
mod decrypt;
#[cfg(target_os = "windows")]
mod exclusive_wasapi;
mod logging;
mod player;
mod player_ipc;
mod preload;
mod state;
mod streaming_buffer;

use oauth2::{PkceCodeChallenge, PkceCodeVerifier};
use player::{Player, PlayerEvent};
use player_ipc::{PlayerIpc, parse_player_ipc};
use serde::{Deserialize, Serialize};
use state::TrackInfo;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tao::{
    event::{ElementState, Event, MouseButton, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    keyboard::Key,
    window::{CursorIcon, ResizeDirection, WindowBuilder},
};
use wry::{WebContext, WebViewBuilder};

#[derive(Deserialize, Debug)]
struct IpcMessage {
    channel: String,
    #[serde(default)]
    args: Vec<serde_json::Value>,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug)]
enum UserEvent {
    Navigate(String),
    IpcMessage(IpcMessage),
    Player(PlayerEvent),
    AutoLoad(TrackInfo),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PkceCredentials {
    credentials_storage_key: String,
    code_challenge: String,
    redirect_uri: String,
    code_verifier: String,
}

fn pkce_credentials_path(data_dir: &Path) -> PathBuf {
    data_dir.join("pkce_credentials.json")
}

fn generate_pkce_credentials() -> PkceCredentials {
    let (code_challenge, code_verifier) = PkceCodeChallenge::new_random_sha256();

    PkceCredentials {
        credentials_storage_key: "tidal".to_string(),
        code_challenge: code_challenge.as_str().to_string(),
        redirect_uri: "tidal://auth/".to_string(),
        code_verifier: code_verifier.secret().to_string(),
    }
}

fn is_valid_pkce_verifier(verifier: &str) -> bool {
    let len = verifier.len();
    (43..=128).contains(&len)
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

fn is_valid_pkce_credentials(c: &PkceCredentials) -> bool {
    if c.credentials_storage_key.trim().is_empty()
        || c.code_challenge.trim().is_empty()
        || c.redirect_uri.trim().is_empty()
        || c.code_verifier.trim().is_empty()
        || !is_valid_pkce_verifier(&c.code_verifier)
    {
        return false;
    }

    let verifier = PkceCodeVerifier::new(c.code_verifier.clone());
    let expected_challenge = PkceCodeChallenge::from_code_verifier_sha256(&verifier);
    c.code_challenge == expected_challenge.as_str()
}

fn load_or_create_pkce_credentials(data_dir: &Path) -> PkceCredentials {
    let path = pkce_credentials_path(data_dir);

    if let Ok(bytes) = std::fs::read(&path)
        && let Ok(mut loaded) = serde_json::from_slice::<PkceCredentials>(&bytes)
    {
        if loaded.credentials_storage_key.trim().is_empty() {
            loaded.credentials_storage_key = "tidal".to_string();
        }
        if loaded.redirect_uri.trim().is_empty() {
            loaded.redirect_uri = "tidal://auth/".to_string();
        }
        if is_valid_pkce_credentials(&loaded) {
            crate::vprintln!("[PKCE]   Loaded persisted credentials");
            return loaded;
        }
        crate::vprintln!("[PKCE]   Invalid persisted credentials, regenerating");
    }

    let generated = generate_pkce_credentials();
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "[PKCE]   Failed to create directory {}: {e}",
            parent.display()
        );
        return generated;
    }
    match serde_json::to_vec_pretty(&generated) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                eprintln!("[PKCE]   Failed to persist credentials: {e}");
            } else {
                crate::vprintln!("[PKCE]   Persisted credentials");
            }
        }
        Err(e) => eprintln!("[PKCE]   Failed to serialize credentials: {e}"),
    }
    generated
}

fn resize_direction_for_point(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    border: f64,
) -> Option<ResizeDirection> {
    let top = y <= border;
    let bottom = y >= height - border;
    let left = x <= border;
    let right = x >= width - border;

    if top && left {
        Some(ResizeDirection::NorthWest)
    } else if top && right {
        Some(ResizeDirection::NorthEast)
    } else if bottom && left {
        Some(ResizeDirection::SouthWest)
    } else if bottom && right {
        Some(ResizeDirection::SouthEast)
    } else if top {
        Some(ResizeDirection::North)
    } else if bottom {
        Some(ResizeDirection::South)
    } else if left {
        Some(ResizeDirection::West)
    } else if right {
        Some(ResizeDirection::East)
    } else {
        None
    }
}

fn is_near_resize_edge(x: f64, y: f64, width: f64, height: f64, hot_zone: f64) -> bool {
    x <= hot_zone || x >= width - hot_zone || y <= hot_zone || y >= height - hot_zone
}

fn cursor_icon_for_resize_direction(direction: ResizeDirection) -> CursorIcon {
    match direction {
        ResizeDirection::North => CursorIcon::NResize,
        ResizeDirection::South => CursorIcon::SResize,
        ResizeDirection::East => CursorIcon::EResize,
        ResizeDirection::West => CursorIcon::WResize,
        ResizeDirection::NorthEast => CursorIcon::NeResize,
        ResizeDirection::NorthWest => CursorIcon::NwResize,
        ResizeDirection::SouthEast => CursorIcon::SeResize,
        ResizeDirection::SouthWest => CursorIcon::SwResize,
    }
}

fn value_trimmed_string(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn first_trimmed_string(obj: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(value_trimmed_string))
}

fn parse_media_item_artist(obj: &serde_json::Value) -> String {
    if let Some(artist) = obj.get("artist") {
        if let Some(name) = value_trimmed_string(artist) {
            return name;
        }
        if let Some(name) = artist.get("name").and_then(value_trimmed_string) {
            return name;
        }
    }

    if let Some(artists) = obj.get("artists").and_then(|v| v.as_array()) {
        let names: Vec<String> = artists
            .iter()
            .filter_map(|artist| {
                value_trimmed_string(artist)
                    .or_else(|| artist.get("name").and_then(value_trimmed_string))
            })
            .collect();
        if !names.is_empty() {
            return names.join(", ");
        }
    }

    String::new()
}

fn parse_track_metadata(payload: &serde_json::Value) -> crate::state::TrackMetadata {
    let title = first_trimmed_string(payload, &["title", "name"]).unwrap_or_default();
    let quality = first_trimmed_string(payload, &["audioQuality", "quality"]).unwrap_or_default();
    let artist = parse_media_item_artist(payload);

    crate::state::TrackMetadata {
        title,
        artist,
        quality,
    }
}

#[derive(Serialize, Debug)]
struct PlayerBridgeEvent {
    t: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    seq: Option<u32>,
    v: serde_json::Value,
}

impl PlayerBridgeEvent {
    fn time(value: f64, seq: u32) -> Self {
        Self {
            t: "time",
            seq: Some(seq),
            v: serde_json::json!(value),
        }
    }

    fn duration(value: f64, seq: u32) -> Self {
        Self {
            t: "duration",
            seq: Some(seq),
            v: serde_json::json!(value),
        }
    }

    fn state(value: &'static str, seq: u32) -> Self {
        Self {
            t: "state",
            seq: Some(seq),
            v: serde_json::json!(value),
        }
    }

    fn devices(value: serde_json::Value) -> Self {
        Self {
            t: "devices",
            seq: None,
            v: value,
        }
    }
}

fn flush_player_bridge(
    webview: &wry::WebView,
    pending_time_update: &mut Option<(f64, u32)>,
    pending_player_events: &mut Vec<PlayerBridgeEvent>,
    pending_misc_js: &mut Vec<String>,
) {
    if let Some((time, seq)) = pending_time_update.take() {
        pending_player_events.push(PlayerBridgeEvent::time(time, seq));
    }

    if !pending_player_events.is_empty() {
        match serde_json::to_string(&*pending_player_events) {
            Ok(events_json) => {
                let js = format!(
                    "if (window.__TIDAL_RS_PLAYER_PUSH__) {{ window.__TIDAL_RS_PLAYER_PUSH__({}); }}",
                    events_json
                );
                let _ = webview.evaluate_script(&js);
            }
            Err(e) => eprintln!("[BRIDGE] Failed to serialize player events: {e}"),
        }
        pending_player_events.clear();
    }

    if !pending_misc_js.is_empty() {
        let js_batch = pending_misc_js.join(";");
        let _ = webview.evaluate_script(&js_batch);
        pending_misc_js.clear();
    }
}

fn main() -> wry::Result<()> {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let window = WindowBuilder::new()
        .with_title("tidal-rs")
        .with_decorations(cfg!(target_os = "linux"))
        .build(&event_loop)
        .unwrap();

    let proxy = event_loop.create_proxy();
    let proxy_nav = proxy.clone();
    let proxy_new_window = proxy.clone();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let rt_handle = rt.handle().clone();

    // Force-initialize the bandwidth governor while a runtime context is active.
    // Lazy::new calls spawn_governor() which requires tokio::spawn.
    {
        let _guard = rt.enter();
        let _ = &*crate::state::GOVERNOR;
    }

    let proxy_player = proxy.clone();
    let proxy_autoload = proxy.clone();
    let player = Arc::new(
        Player::new(
            move |event| {
                let _ = proxy_player.send_event(UserEvent::Player(event));
            },
            rt_handle.clone(),
        )
        .expect("Failed to initialize player"),
    );
    let player_clone = player.clone();

    let data_dir = {
        #[cfg(target_os = "windows")]
        {
            let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
            std::path::PathBuf::from(base).join("tidal-rs")
        }
        #[cfg(not(target_os = "windows"))]
        {
            let base = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            std::path::PathBuf::from(base)
                .join(".local")
                .join("share")
                .join("tidal-rs")
        }
    };

    let pkce_credentials = load_or_create_pkce_credentials(&data_dir);
    let pkce_credentials_json = serde_json::to_string(&pkce_credentials).unwrap_or_else(|e| {
        eprintln!("[PKCE]   Failed to encode credentials for JS: {e}");
        "{\"credentialsStorageKey\":\"tidal\",\"codeChallenge\":\"\",\"redirectUri\":\"tidal://auth/\",\"codeVerifier\":\"\"}".to_string()
    });

    let mut web_context = WebContext::new(Some(data_dir));

    let script = include_str!(concat!(env!("OUT_DIR"), "/bundle.js"));
    let initial_is_maximized = window.is_maximized();
    let initial_is_fullscreen = window.fullscreen().is_some();

    let builder = WebViewBuilder::new_with_web_context(&mut web_context)
        .with_url("https://desktop.tidal.com/")
        .with_devtools(true)
        .with_initialization_script(format!(
            r#"window.__TIDAL_RS_PLATFORM__ = '{platform}';
window.__TIDAL_RS_WINDOW_STATE__ = {{
    isMaximized: {initial_is_maximized},
    isFullscreen: {initial_is_fullscreen}
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
}});"#,
            platform = if cfg!(target_os = "linux") {
                "linux"
            } else if cfg!(target_os = "macos") {
                "darwin"
            } else {
                "win32"
            },
            initial_is_maximized = initial_is_maximized,
            initial_is_fullscreen = initial_is_fullscreen,
            pkce_credentials_json = pkce_credentials_json
        ))
        .with_initialization_script(script)
        // TODO: temporary Linux UA to bypass Tidal's anti-bot blocking during login.
        // The Windows Electron UA triggers a fingerprint mismatch on WebKitGTK.
        .with_user_agent({
            #[cfg(target_os = "linux")]
            { "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) tidal-hifi/1.12.4-beta Chrome/144.0.7559.96 Electron/40.1.0 Safari/537.36" }
            #[cfg(not(target_os = "linux"))]
            { "Mozilla/5.0 (Windows NT 10.0; WOW64) AppleWebKit/537.36 (KHTML, like Gecko) TIDAL/1.12.4-beta Chrome/142.0.7444.235 Electron/39.2.7 Safari/537.36" }
        })
        .with_ipc_handler(move |req| {
            let s = req.body();
            if let Ok(msg) = serde_json::from_str::<IpcMessage>(s) {
                let _ = proxy.send_event(UserEvent::IpcMessage(msg));
            } else {
                crate::vprintln!("Received unknown IPC message: {}", s);
            }
        })
        .with_navigation_handler(move |url| {
            if url.starts_with("tidal://") {
                let _ = proxy_nav.send_event(UserEvent::Navigate(url));
                return false;
            }
            true
        })
        .with_new_window_req_handler(move |url, _features| {
            if url.starts_with("tidal://") {
                let _ = proxy_new_window.send_event(UserEvent::Navigate(url));
                return wry::NewWindowResponse::Deny;
            }
            wry::NewWindowResponse::Allow
        });

    #[cfg(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "ios",
        target_os = "android"
    ))]
    let webview = builder.build(&window)?;

    #[cfg(not(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "ios",
        target_os = "android"
    )))]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let vbox = window.default_vbox().unwrap();
        builder.build_gtk(vbox)?
    };

    let mut pending_navigation = std::env::args().find(|arg| arg.starts_with("tidal://"));
    let update_window_state = |webview: &wry::WebView, window: &tao::window::Window| {
        let is_maximized = window.is_maximized();
        let is_fullscreen = window.fullscreen().is_some();
        let js = format!(
            "if (window.__TIDAL_CALLBACKS__ && window.__TIDAL_CALLBACKS__.window && window.__TIDAL_CALLBACKS__.window.updateState) {{ window.__TIDAL_CALLBACKS__.window.updateState({}, {}); }}",
            is_maximized, is_fullscreen
        );
        let _ = webview.evaluate_script(&js);
    };

    let mut pending_time_update: Option<(f64, u32)> = None;
    let mut pending_player_events: Vec<PlayerBridgeEvent> = Vec::new();
    let mut pending_misc_js: Vec<String> = Vec::new();
    let mut player_flush_deadline: Option<Instant> = None;
    let player_flush_interval = Duration::from_millis(24);
    let frameless = !cfg!(target_os = "linux");
    let resize_border_px = 6.0_f64;
    let resize_hot_zone_px = resize_border_px + 8.0;
    let mut last_cursor_pos: Option<(f64, f64)> = None;
    let mut last_resize_direction: Option<ResizeDirection> = None;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                let now = Instant::now();
                if player_flush_deadline.is_some_and(|t| now >= t) {
                    flush_player_bridge(
                        &webview,
                        &mut pending_time_update,
                        &mut pending_player_events,
                        &mut pending_misc_js,
                    );
                    player_flush_deadline = None;
                }
            }
            Event::MainEventsCleared => {
                let now = Instant::now();
                if player_flush_deadline.is_some_and(|t| now >= t) {
                    flush_player_bridge(
                        &webview,
                        &mut pending_time_update,
                        &mut pending_player_events,
                        &mut pending_misc_js,
                    );
                    player_flush_deadline = None;
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            Event::WindowEvent {
                event:
                    WindowEvent::KeyboardInput {
                        event:
                            tao::event::KeyEvent {
                                logical_key: Key::F12,
                                state: ElementState::Pressed,
                                ..
                            },
                        ..
                    },
                ..
            } => {
                webview.open_devtools();
            }
            Event::WindowEvent {
                event: WindowEvent::Resized(_) | WindowEvent::Moved(_),
                ..
            } => {
                update_window_state(&webview, &window);
            }
            Event::WindowEvent {
                event: WindowEvent::CursorMoved { position, .. },
                ..
            } => {
                if frameless {
                    let x = position.x;
                    let y = position.y;
                    last_cursor_pos = Some((x, y));

                    if window.is_maximized() {
                        if last_resize_direction.take().is_some() {
                            window.set_cursor_icon(CursorIcon::Default);
                        }
                    } else {
                        let size = window.inner_size();
                        let width = size.width as f64;
                        let height = size.height as f64;
                        let direction =
                            if is_near_resize_edge(x, y, width, height, resize_hot_zone_px) {
                                resize_direction_for_point(x, y, width, height, resize_border_px)
                            } else {
                                None
                            };

                        if direction != last_resize_direction {
                            last_resize_direction = direction;
                            window.set_cursor_icon(
                                direction
                                    .map(cursor_icon_for_resize_direction)
                                    .unwrap_or(CursorIcon::Default),
                            );
                        }
                    }
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CursorLeft { .. },
                ..
            } => {
                last_cursor_pos = None;
                if frameless && last_resize_direction.take().is_some() {
                    window.set_cursor_icon(CursorIcon::Default);
                }
            }
            Event::WindowEvent {
                event:
                    WindowEvent::MouseInput {
                        state: ElementState::Pressed,
                        button: MouseButton::Left,
                        ..
                    },
                ..
            } => {
                if frameless
                    && !window.is_maximized()
                    && let Some((x, y)) = last_cursor_pos
                {
                    let size = window.inner_size();
                    let width = size.width as f64;
                    let height = size.height as f64;
                    if let Some(direction) =
                        resize_direction_for_point(x, y, width, height, resize_border_px)
                        && let Err(e) = window.drag_resize_window(direction)
                    {
                        eprintln!("[WINDOW] drag_resize_window failed: {e}");
                    }
                }
            }
            Event::UserEvent(user_event) => match user_event {
                UserEvent::Player(player_event) => {
                    match player_event {
                        PlayerEvent::TimeUpdate(time, seq) => {
                            pending_time_update = Some((time, seq));
                            let now = Instant::now();
                            if time == 0.0 {
                                // Reset events should be reflected right away.
                                player_flush_deadline = Some(now);
                            } else if player_flush_deadline.is_none() {
                                player_flush_deadline = Some(now + player_flush_interval);
                            }
                            if let Some(deadline) = player_flush_deadline {
                                *control_flow = ControlFlow::WaitUntil(deadline);
                            }
                        }
                        PlayerEvent::StateChange(state, seq) => {
                            pending_player_events.push(PlayerBridgeEvent::state(state, seq));

                            if state == "completed" {
                                let proxy_autoload = proxy_autoload.clone();
                                rt_handle.spawn(async move {
                                    if let Some(track) = preload::next_preloaded_track().await {
                                        let _ =
                                            proxy_autoload.send_event(UserEvent::AutoLoad(track));
                                    }
                                });
                            }
                            let now = Instant::now();
                            if player_flush_deadline.is_none() {
                                player_flush_deadline = Some(now + player_flush_interval);
                            }
                            if let Some(deadline) = player_flush_deadline {
                                *control_flow = ControlFlow::WaitUntil(deadline);
                            }
                        }
                        PlayerEvent::Duration(duration, seq) => {
                            pending_player_events.push(PlayerBridgeEvent::duration(duration, seq));
                            let now = Instant::now();
                            if player_flush_deadline.is_none() {
                                player_flush_deadline = Some(now + player_flush_interval);
                            }
                            if let Some(deadline) = player_flush_deadline {
                                *control_flow = ControlFlow::WaitUntil(deadline);
                            }
                        }
                        PlayerEvent::AudioDevices(devices, req_id) => {
                            if let Ok(json_devices) = serde_json::to_string(&devices) {
                                if let Some(id) = req_id {
                                    let js = format!(
                                        "window.__TIDAL_IPC_RESPONSE__('{}', null, {})",
                                        id, json_devices
                                    );
                                    pending_misc_js.push(js);
                                } else {
                                    pending_player_events.push(PlayerBridgeEvent::devices(
                                        serde_json::json!(devices),
                                    ));
                                }
                                let now = Instant::now();
                                if player_flush_deadline.is_none() {
                                    player_flush_deadline = Some(now + player_flush_interval);
                                }
                                if let Some(deadline) = player_flush_deadline {
                                    *control_flow = ControlFlow::WaitUntil(deadline);
                                }
                            }
                        }
                    }
                }
                UserEvent::AutoLoad(track) => {
                    if let Err(e) = player_clone.load(track.url, "flac".to_string(), track.key) {
                        eprintln!("Failed to auto-load preloaded track: {}", e);
                    }
                }
                UserEvent::Navigate(url) => {
                    crate::vprintln!("Navigating to: {}", url);
                    if url.starts_with("tidal://") {
                        let path = url.strip_prefix("tidal://").unwrap();
                        let _ = webview.load_url(&format!("https://desktop.tidal.com/{}", path));
                    }
                }
                UserEvent::IpcMessage(msg) => {
                    crate::vprintln!("IPC Message: {:?}", msg);
                    if msg.channel.starts_with("player.") {
                        match parse_player_ipc(&msg.channel, &msg.args, msg.id.as_deref()) {
                            Ok(player_ipc) => match player_ipc {
                                PlayerIpc::Load { url, format, key } => {
                                    if let Err(e) = player_clone.load(url, format, key) {
                                        eprintln!("Failed to load track: {}", e);
                                    }
                                }
                                PlayerIpc::Recover {
                                    url,
                                    format,
                                    key,
                                    target_time,
                                } => {
                                    if let Err(e) =
                                        player_clone.recover(url, format, key, target_time)
                                    {
                                        eprintln!("Failed to recover track: {}", e);
                                    }
                                }
                                PlayerIpc::Preload { url, key } => {
                                    let track = crate::state::TrackInfo { url, key };
                                    rt_handle.spawn(async move {
                                        preload::start_preload(track).await;
                                    });
                                }
                                PlayerIpc::PreloadCancel => {
                                    rt_handle.spawn(async {
                                        preload::cancel_preload().await;
                                    });
                                }
                                PlayerIpc::Metadata { payload } => {
                                    let meta = parse_track_metadata(&payload);
                                    let mut lock = crate::state::CURRENT_METADATA.lock().unwrap();
                                    *lock = Some(meta);
                                }
                                PlayerIpc::Play => {
                                    let _ = player_clone.play();
                                }
                                PlayerIpc::Pause => {
                                    let _ = player_clone.pause();
                                }
                                PlayerIpc::Stop => {
                                    let _ = player_clone.stop();
                                }
                                PlayerIpc::Seek { time } => {
                                    let _ = player_clone.seek(time);
                                }
                                PlayerIpc::Volume { volume } => {
                                    let _ = player_clone.set_volume(volume);
                                }
                                PlayerIpc::DevicesGet { request_id } => {
                                    let _ = player_clone.get_audio_devices(request_id);
                                }
                                PlayerIpc::DevicesSet { id, exclusive } => {
                                    let _ = player_clone.set_audio_device(id, exclusive);
                                }
                            },
                            Err(e) => {
                                crate::vprintln!(
                                    "[IPC]    Invalid player IPC ({}): {:?}",
                                    msg.channel,
                                    e
                                );
                            }
                        }
                    } else {
                        match msg.channel.as_str() {
                            "window.devtools" => {
                                webview.open_devtools();
                            }
                            "window.drag" => {
                                let _ = window.drag_window();
                            }
                            "window.close" => *control_flow = ControlFlow::Exit,
                            "window.maximize" => {
                                window.set_maximized(true);
                                update_window_state(&webview, &window);
                            }
                            "window.minimize" => {
                                window.set_minimized(true);
                                update_window_state(&webview, &window);
                            }
                            "window.unmaximize" => {
                                window.set_maximized(false);
                                update_window_state(&webview, &window);
                            }
                            "web.loaded" => {
                                if let Some(url) = pending_navigation.take()
                                    && url.starts_with("tidal://")
                                {
                                    let path = url.strip_prefix("tidal://").unwrap();
                                    let _ = webview
                                        .load_url(&format!("https://desktop.tidal.com/{}", path));
                                }
                                update_window_state(&webview, &window);
                            }
                            _ => {}
                        }
                    }
                }
            },
            _ => (),
        }
    });
}
