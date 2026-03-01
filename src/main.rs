#![cfg_attr(
    all(not(debug_assertions), not(feature = "console")),
    windows_subsystem = "windows"
)]
mod bandwidth;
mod decrypt;
#[cfg(target_os = "windows")]
mod exclusive_wasapi;
mod player;
mod preload;
mod state;
mod streaming_buffer;

use player::{Player, PlayerEvent};
use serde::Deserialize;
use state::TrackInfo;
use std::sync::Arc;
use tao::{
    event::{ElementState, Event, WindowEvent},
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

    let mut web_context = WebContext::new(Some(data_dir));

    let script = include_str!(concat!(env!("OUT_DIR"), "/bundle.js"));

    let builder = WebViewBuilder::new_with_web_context(&mut web_context)
        .with_url("https://desktop.tidal.com/")
        .with_devtools(true)
        .with_initialization_script(format!(
            r#"window.__TIDAL_RS_PLATFORM__ = '{platform}';
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
            }
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
                println!("Received unknown IPC message: {}", s);
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

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
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
            Event::UserEvent(user_event) => match user_event {
                 UserEvent::Player(player_event) => {
                     match player_event {
                        PlayerEvent::TimeUpdate(time) => {
                            let js = format!(
                                 "if (window.NativePlayerComponent && window.NativePlayerComponent.trigger) {{ window.NativePlayerComponent.trigger('mediacurrenttime', {}); }}",
                                 time
                             );
                             let _ = webview.evaluate_script(&js);
                        },
                         PlayerEvent::StateChange(state) => {
                             let js = format!(
                                 "if (window.NativePlayerComponent && window.NativePlayerComponent.trigger) {{ window.NativePlayerComponent.trigger('mediastate', '{}'); }}",
                                 state
                             );
                             let _ = webview.evaluate_script(&js);

                             if state == "completed" {
                                 let proxy_autoload = proxy_autoload.clone();
                                 rt_handle.spawn(async move {
                                     if let Some(track) = preload::next_preloaded_track().await {
                                         let _ = proxy_autoload.send_event(UserEvent::AutoLoad(track));
                                     }
                                 });
                             }
                         },
                         PlayerEvent::Duration(duration) => {
                                let js = format!(
                                    "if (window.NativePlayerComponent && window.NativePlayerComponent.trigger) {{ window.NativePlayerComponent.trigger('mediaduration', {}); }}",
                                    duration
                                );
                                let _ = webview.evaluate_script(&js);
                            },
                        PlayerEvent::AudioDevices(devices, req_id) => {
                             if let Ok(json_devices) = serde_json::to_string(&devices) {
                                 if let Some(id) = req_id {
                                     let js = format!("window.__TIDAL_IPC_RESPONSE__('{}', null, {})", id, json_devices);
                                     let _ = webview.evaluate_script(&js);
                                 } else {
                                      let js = format!(
                                          "if (window.NativePlayerComponent && window.NativePlayerComponent.trigger) {{ window.NativePlayerComponent.trigger('devices', {}); }}",
                                          json_devices
                                      );
                                      let _ = webview.evaluate_script(&js);
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
                    println!("Navigating to: {}", url);
                    if url.starts_with("tidal://") {
                        let path = url.strip_prefix("tidal://").unwrap();
                        let _ = webview.load_url(
                            &format!("https://desktop.tidal.com/{}", path),
                        );
                    }
                }
                UserEvent::IpcMessage(msg) => {
                    println!("IPC Message: {:?}", msg);
                    match msg.channel.as_str() {
                        "player.load" => {
                             if let (Some(url), Some(format), Some(key)) = (
                                 msg.args.first().and_then(|v| v.as_str()),
                                 msg.args.get(1).and_then(|v| v.as_str()),
                                 msg.args.get(2).and_then(|v| v.as_str())
                             )
                                 && let Err(e) = player_clone.load(url.to_string(), format.to_string(), key.to_string())
                             {
                                 eprintln!("Failed to load track: {}", e);
                             }
                        },
                        "player.preload" => {
                            if let (Some(url), Some(_format), Some(key)) = (
                                msg.args.first().and_then(|v| v.as_str()),
                                msg.args.get(1).and_then(|v| v.as_str()),
                                msg.args.get(2).and_then(|v| v.as_str())
                            ) {
                                let track = crate::state::TrackInfo {
                                    url: url.to_string(),
                                    key: key.to_string(),
                                };
                                rt_handle.spawn(async move {
                                        preload::start_preload(track).await;
                                });
                            }
                        }
                        "player.preload.cancel" => {
                            rt_handle.spawn(async {
                                preload::cancel_preload().await;
                            });
                        }
                        "player.metadata" => {
                            if let Some(obj) = msg.args.first() {
                                let meta = crate::state::TrackMetadata {
                                    title: obj.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                                    artist: obj.get("artist").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                                    quality: obj.get("quality").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                                };
                                let mut lock = crate::state::CURRENT_METADATA.lock().unwrap();
                                *lock = Some(meta);
                            }
                        }
                        "player.play" => { let _ = player_clone.play(); },
                        "player.pause" => { let _ = player_clone.pause(); },
                        "player.stop" => { let _ = player_clone.stop(); },
                        "player.seek" => {
                            if let Some(time) = msg.args.first().and_then(|v| v.as_f64()) {
                                let _ = player_clone.seek(time);
                            }
                        },
                        "player.volume" => {
                            if let Some(vol) = msg.args.first().and_then(|v| v.as_f64()) {
                                let _ = player_clone.set_volume(vol);
                            }
                        },
                        "player.devices.get" => {
                            let _ = player_clone.get_audio_devices(msg.id);
                        },
                        "player.devices.set" => {
                            if let Some(id) = msg.args.first().and_then(|v| v.as_str()) {
                                 let exclusive = msg
                                     .args
                                     .get(1)
                                     .and_then(|v| v.as_str())
                                     .is_some_and(|mode| mode == "exclusive");
                                 let _ = player_clone.set_audio_device(id.to_string(), exclusive);
                            }
                        },
                        "window.devtools" => {
                            webview.open_devtools();
                        }
                        "window.drag" => {
                            let _ = window.drag_window();
                        }
                        "window.resize" => {
                            if window.is_maximized() {
                                // Ignore resize when maximized.
                            } else if let Some(dir) = msg.args.first().and_then(|v| v.as_str()) {
                                let direction = match dir {
                                    "n" => Some(ResizeDirection::North),
                                    "s" => Some(ResizeDirection::South),
                                    "e" => Some(ResizeDirection::East),
                                    "w" => Some(ResizeDirection::West),
                                    "ne" => Some(ResizeDirection::NorthEast),
                                    "nw" => Some(ResizeDirection::NorthWest),
                                    "se" => Some(ResizeDirection::SouthEast),
                                    "sw" => Some(ResizeDirection::SouthWest),
                                    _ => None,
                                };
                                if let Some(d) = direction
                                    && let Err(e) = window.drag_resize_window(d)
                                {
                                    eprintln!("[WINDOW] drag_resize_window failed: {e}");
                                }
                            }
                        }
                        "window.cursor" => {
                            if let Some(dir) = msg.args.first().and_then(|v| v.as_str()) {
                                let cursor = match dir {
                                    "n" => CursorIcon::NResize,
                                    "s" => CursorIcon::SResize,
                                    "e" => CursorIcon::EResize,
                                    "w" => CursorIcon::WResize,
                                    "ne" => CursorIcon::NeResize,
                                    "nw" => CursorIcon::NwResize,
                                    "se" => CursorIcon::SeResize,
                                    "sw" => CursorIcon::SwResize,
                                    _ => CursorIcon::Default,
                                };
                                window.set_cursor_icon(cursor);
                            }
                        }
                        "window.cursor.reset" => {
                            window.set_cursor_icon(CursorIcon::Default);
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
                        "window.state.get" => {
                            if let Some(id) = msg.id {
                                let is_maximized = window.is_maximized();
                                let is_fullscreen = window.fullscreen().is_some();
                                let value = serde_json::json!({
                                    "isMaximized": is_maximized,
                                    "isFullscreen": is_fullscreen
                                });
                                let js = format!(
                                    "window.__TIDAL_IPC_RESPONSE__('{}', null, {})",
                                    id, value
                                );
                                let _ = webview.evaluate_script(&js);
                            }
                        }
                        "web.loaded" => {
                            if let Some(url) = pending_navigation.take()
                                && url.starts_with("tidal://")
                            {
                                let path = url.strip_prefix("tidal://").unwrap();
                                let _ = webview.load_url(
                                    &format!("https://desktop.tidal.com/{}", path),
                                );
                            }
                            update_window_state(&webview, &window);
                        }
                        _ => {}
                    }
                }
            },
            _ => (),
        }
    });
}
