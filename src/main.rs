#![cfg_attr(
    all(not(debug_assertions), not(feature = "console")),
    windows_subsystem = "windows"
)]
mod auth;
mod bandwidth;
mod bridge;
mod decrypt;
mod flac_meta;
mod js_runtime;
mod logging;
mod media_controls;
mod metadata;
mod player;
mod plugins;
mod preload;
mod settings;
mod state;
#[cfg(target_os = "windows")]
mod thumbbar;

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
use player::{PlaybackState, Player, PlayerEvent};
use serde::Deserialize;
use state::TrackInfo;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub(crate) type IpcCallback = Arc<Mutex<dyn cef::wrapper::message_router::BrowserSideCallback>>;

#[derive(Deserialize, Debug)]
pub(crate) struct IpcMessage {
    pub(crate) channel: String,
    #[serde(default)]
    pub(crate) args: Vec<serde_json::Value>,
    #[serde(default)]
    pub(crate) id: Option<String>,
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
    #[allow(dead_code)]
    app_settings: settings::Settings,
    // The main browser reference – set once on_after_created fires
    browser: Option<Browser>,
    /// Prevents scheduling duplicate delayed flush tasks.
    flush_scheduled: bool,
    media_controls: Option<media_controls::OsMediaControls>,
    media_duration: Option<f64>,
    plugin_store: plugins::PluginStore,
    pending_ipc_callbacks: HashMap<String, IpcCallback>,
    /// QuickJS plugin runtime — executes TidaLuna plugins outside CEF
    plugin_runtime: Option<js_runtime::plugin_runtime::PluginRuntime>,
    #[cfg(target_os = "windows")]
    thumbbar: Option<thumbbar::ThumbBar>,
}

// SAFETY: AppState is only accessed on the CEF UI thread (single-threaded access)
// except for `player` and `rt_handle` which are Send+Sync.
// The Arc<Mutex<>> is needed for the borrow checker at callback boundaries,
// but actual concurrent access does not occur.
unsafe impl Send for AppState {}
unsafe impl Sync for AppState {}

static APP_STATE: std::sync::OnceLock<Arc<Mutex<AppState>>> = std::sync::OnceLock::new();

pub(crate) fn with_state<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut AppState) -> R,
{
    APP_STATE.get().map(|s| {
        let mut guard = s.lock().expect("AppState lock poisoned");
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
#[allow(dead_code)]
pub(crate) fn eval_js(js: &str) {
    with_state(|state| {
        if let Some(ref browser) = state.browser
            && let Some(frame) = browser.main_frame()
        {
            exec_js_on_frame(&frame, js);
        }
    });
}

/// Open a path or URL with the OS default handler.
fn open_in_os(target: impl AsRef<std::ffi::OsStr>) {
    let target = target.as_ref();
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args([
                std::ffi::OsStr::new("/C"),
                std::ffi::OsStr::new("start"),
                std::ffi::OsStr::new(""),
                target,
            ])
            .spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(target).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(target).spawn();
    }
}

fn toggle_devtools() {
    with_state(|state| {
        if let Some(ref browser) = state.browser
            && let Some(host) = browser.host()
        {
            if host.has_dev_tools() == 1 {
                host.close_dev_tools();
            } else {
                host.show_dev_tools(None, None, None, None);
            }
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
        let dominated_by_time = msg
            .args
            .first()
            .and_then(|v| v.as_str())
            .is_some_and(|s| s == "setCurrentTime");
        if !dominated_by_time {
            crate::vprintln!("[JS-DBG] {:?}", msg.args);
        }
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
                PlayerIpc::LoadDash {
                    init_url,
                    segment_urls,
                    format,
                } => {
                    if let Err(e) = state.player.load_dash(init_url, segment_urls, format) {
                        eprintln!("Failed to load DASH track: {}", e);
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
                PlayerIpc::Preload { url, format, key } => {
                    let track = TrackInfo { url, format, key };
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
                    if let Some(ref mut mc) = state.media_controls {
                        mc.set_metadata(&meta.title, &meta.artist, state.media_duration);
                    }
                    let mut lock = crate::state::CURRENT_METADATA
                        .lock()
                        .expect("CURRENT_METADATA lock poisoned");
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

// ---------------------------------------------------------------------------
// Plugin IPC handler — responds via cefQuery callback (not exec_js)
// ---------------------------------------------------------------------------

pub(crate) fn ipc_callback_ok(cb: &IpcCallback, result: &str) {
    cb.lock()
        .expect("IPC callback lock poisoned")
        .success_str(&format!("S:{result}"));
}

pub(crate) fn ipc_callback_err(cb: &IpcCallback, error: &str) {
    cb.lock()
        .expect("IPC callback lock poisoned")
        .success_str(&format!("E:{error}"));
}

fn handle_plugin_ipc(msg: IpcMessage, callback: IpcCallback) {
    // jsrt.eval — evaluate JS in the QuickJS plugin runtime (debug/test)
    if msg.channel == "jsrt.eval" {
        let code = msg
            .args
            .first()
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let result = with_state(|state| {
            state
                .plugin_runtime
                .as_ref()
                .and_then(|rt| rt.eval(&code).ok())
                .unwrap_or_else(|| "null".to_string())
        })
        .unwrap_or_else(|| "null".to_string());
        ipc_callback_ok(&callback, &result);
        return;
    }

    // jsrt.load_plugins — load all enabled plugins in the QuickJS runtime
    if msg.channel == "jsrt.load_plugins" {
        let loaded = with_state(|state| {
            if let Some(ref mut rt) = state.plugin_runtime {
                let urls = rt.load_all_enabled(&state.plugin_store);
                serde_json::to_string(&urls).unwrap_or_else(|_| "[]".to_string())
            } else {
                "[]".to_string()
            }
        })
        .unwrap_or_else(|| "[]".to_string());
        ipc_callback_ok(&callback, &loaded);
        return;
    }

    // jsrt.tick — drive timers and pending jobs in the QuickJS runtime
    if msg.channel == "jsrt.tick" {
        with_state(|state| {
            if let Some(ref rt) = state.plugin_runtime {
                rt.tick();
            }
        });
        ipc_callback_ok(&callback, "true");
        return;
    }

    // player.parse_dash — synchronous DASH MPD XML parsing
    if msg.channel == "player.parse_dash" {
        let xml = msg
            .args
            .first()
            .and_then(|v| v.as_str())
            .unwrap_or("");
        match crate::player::dash::parse_dash_mpd(xml) {
            Ok(manifest) => {
                let json = serde_json::to_string(&manifest).unwrap_or_else(|_| "null".into());
                ipc_callback_ok(&callback, &json);
            }
            Err(e) => ipc_callback_err(&callback, &format!("{e:#}")),
        }
        return;
    }

    // proxy.fetch — async HTTP fetch bypassing browser CORS
    // Args: [url, optional_headers_json]
    if msg.channel == "proxy.fetch" {
        let url = msg
            .args
            .first()
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let headers_json = msg
            .args
            .get(1)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let id = msg.id.unwrap_or_default();
        with_state(|state| {
            state.pending_ipc_callbacks.insert(id.clone(), callback);
            let rt = state.rt_handle.clone();
            rt.spawn(async move {
                handle_proxy_fetch(id, url, headers_json).await;
            });
        });
        return;
    }

    // proxy.head — async HTTP HEAD request, returns status + content-length
    // Args: [url]
    if msg.channel == "proxy.head" {
        let url = msg
            .args
            .first()
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let id = msg.id.unwrap_or_default();
        with_state(|state| {
            state.pending_ipc_callbacks.insert(id.clone(), callback);
            let rt = state.rt_handle.clone();
            rt.spawn(async move {
                handle_proxy_head(id, url).await;
            });
        });
        return;
    }

    // plugin.fetch_package — async fetch of {url}.json manifest only (no install)
    if msg.channel == "plugin.fetch_package" {
        let url = msg
            .args
            .first()
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let id = msg.id.unwrap_or_default();
        with_state(|state| {
            state.pending_ipc_callbacks.insert(id.clone(), callback);
            let rt = state.rt_handle.clone();
            rt.spawn(async move {
                let client = &*crate::state::HTTP_CLIENT;
                let result = fetch_plugin_package(client, &url).await;
                with_state(|state| {
                    let Some(cb) = state.pending_ipc_callbacks.remove(&id) else {
                        return;
                    };
                    match result {
                        Ok(manifest) => ipc_callback_ok(&cb, &manifest),
                        Err(e) => ipc_callback_err(&cb, &format!("{e:#}")),
                    }
                });
            });
        });
        return;
    }

    // plugin.install is async (HTTP fetch) — store callback, respond later
    if msg.channel == "plugin.install" {
        let url = msg
            .args
            .first()
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let id = msg.id.unwrap_or_default();
        with_state(|state| {
            state.pending_ipc_callbacks.insert(id.clone(), callback);
            let rt = state.rt_handle.clone();
            rt.spawn(async move {
                handle_plugin_install(id, url).await;
            });
        });
        return;
    }

    with_state(|state| match msg.channel.as_str() {
        "plugin.list" => {
            let plugins = state.plugin_store.list();
            let json = serde_json::to_string(&plugins).unwrap_or_else(|_| "[]".to_string());
            ipc_callback_ok(&callback, &json);
        }
        "plugin.get_code" => {
            let url = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            match state.plugin_store.get_code(url) {
                Some(code) => {
                    let json = serde_json::to_string(&code).unwrap_or_else(|_| "null".to_string());
                    ipc_callback_ok(&callback, &json);
                }
                None => ipc_callback_ok(&callback, "null"),
            }
        }
        "plugin.uninstall" => {
            let url = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            match state.plugin_store.uninstall(url) {
                Ok(()) => ipc_callback_ok(&callback, "true"),
                Err(e) => ipc_callback_err(&callback, &e.to_string()),
            }
        }
        "plugin.uninstall_all" => match state.plugin_store.uninstall_all() {
            Ok(()) => ipc_callback_ok(&callback, "true"),
            Err(e) => ipc_callback_err(&callback, &e.to_string()),
        },
        "plugin.enable" => {
            let url = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            match state.plugin_store.enable(url) {
                Ok(()) => ipc_callback_ok(&callback, "true"),
                Err(e) => ipc_callback_err(&callback, &e.to_string()),
            }
        }
        "plugin.disable" => {
            let url = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            match state.plugin_store.disable(url) {
                Ok(()) => ipc_callback_ok(&callback, "true"),
                Err(e) => ipc_callback_err(&callback, &e.to_string()),
            }
        }
        "plugin.storage.get" => {
            let ns = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            let key = msg.args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            match state.plugin_store.storage_get(ns, key) {
                Some(val) => {
                    let json = serde_json::to_string(&val).unwrap_or_else(|_| "null".to_string());
                    ipc_callback_ok(&callback, &json);
                }
                None => ipc_callback_ok(&callback, "null"),
            }
        }
        "plugin.storage.set" => {
            let ns = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            let key = msg.args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            let value = msg.args.get(2).and_then(|v| v.as_str()).unwrap_or("");
            match state.plugin_store.storage_set(ns, key, value) {
                Ok(()) => ipc_callback_ok(&callback, "true"),
                Err(e) => ipc_callback_err(&callback, &e.to_string()),
            }
        }
        "plugin.storage.del" => {
            let ns = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            let key = msg.args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            match state.plugin_store.storage_del(ns, key) {
                Ok(()) => ipc_callback_ok(&callback, "true"),
                Err(e) => ipc_callback_err(&callback, &e.to_string()),
            }
        }
        "plugin.storage.keys" => {
            let ns = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            let keys = state.plugin_store.storage_keys(ns);
            let json = serde_json::to_string(&keys).unwrap_or_else(|_| "[]".to_string());
            ipc_callback_ok(&callback, &json);
        }
        _ => {
            ipc_callback_err(
                &callback,
                &format!("Unknown plugin channel: {}", msg.channel),
            );
        }
    });
}

async fn handle_proxy_head(id: String, url: String) {
    let client = &*crate::state::HTTP_CLIENT;
    let result = client.head(&url).send().await;

    with_state(|state| {
        let Some(callback) = state.pending_ipc_callbacks.remove(&id) else {
            return;
        };
        match result {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let content_length = resp.content_length().unwrap_or(0);
                let json = serde_json::json!({
                    "status": status,
                    "contentLength": content_length,
                });
                ipc_callback_ok(&callback, &json.to_string());
            }
            Err(e) => {
                ipc_callback_err(&callback, &format!("proxy.head failed: {e}"));
            }
        }
    });
}

async fn handle_proxy_fetch(id: String, url: String, headers_json: String) {
    let client = &*crate::state::HTTP_CLIENT;
    let mut req = client.get(&url);

    // Apply optional headers from JSON object (e.g. {"Authorization": "Bearer ..."})
    if !headers_json.is_empty()
        && let Ok(headers) =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&headers_json)
    {
        for (key, value) in headers {
            if let Some(val) = value.as_str() {
                req = req.header(&key, val);
            }
        }
    }

    let result = req.send().await;

    with_state(|state| {
        let Some(callback) = state.pending_ipc_callbacks.remove(&id) else {
            return;
        };
        match result {
            Ok(resp) => {
                let status = resp.status().as_u16();
                // Spawn a task to read the body since it's async
                let rt = state.rt_handle.clone();
                rt.spawn(async move {
                    let body = resp.text().await.unwrap_or_default();
                    let json = serde_json::json!({
                        "status": status,
                        "body": body,
                    });
                    ipc_callback_ok(&callback, &json.to_string());
                });
            }
            Err(e) => {
                ipc_callback_err(&callback, &format!("proxy.fetch failed: {e}"));
            }
        }
    });
}

async fn handle_plugin_install(id: String, url: String) {
    let client = &*crate::state::HTTP_CLIENT;
    let result = fetch_plugin_data(client, &url).await;

    with_state(|state| {
        let Some(callback) = state.pending_ipc_callbacks.remove(&id) else {
            return;
        };
        match result {
            Ok((name, manifest, code, hash)) => {
                match state
                    .plugin_store
                    .install(&url, &name, &manifest, &code, &hash)
                {
                    Ok(()) => {
                        let info = plugins::PluginInfo {
                            url: url.clone(),
                            name,
                            manifest,
                            hash: Some(hash),
                            enabled: true,
                            installed: true,
                        };
                        let json =
                            serde_json::to_string(&info).unwrap_or_else(|_| "null".to_string());
                        ipc_callback_ok(&callback, &json);
                    }
                    Err(e) => ipc_callback_err(&callback, &e.to_string()),
                }
            }
            Err(e) => ipc_callback_err(&callback, &format!("{e:#}")),
        }
    });
}

/// Sanitize a plugin URL by stripping known extensions, matching upstream behavior.
fn sanitize_plugin_url(url: &str) -> &str {
    url.trim_end_matches(".mjs.map")
        .trim_end_matches(".mjs")
        .trim_end_matches(".json")
}

/// Fetch just the package manifest (`{base_url}.json`). Used by `plugin.fetch_package` IPC.
async fn fetch_plugin_package(client: &reqwest::Client, url: &str) -> anyhow::Result<String> {
    let base = sanitize_plugin_url(url);
    let json_url = format!("{base}.json");
    let manifest_str = client
        .get(&json_url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    // Validate it's actual JSON
    let _: serde_json::Value = serde_json::from_str(&manifest_str)?;
    Ok(manifest_str)
}

async fn fetch_plugin_data(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<(String, String, String, String)> {
    use std::hash::{Hash, Hasher};

    let base = sanitize_plugin_url(url);

    // Try upstream pattern first: {base}.json for manifest, {base}.mjs for code
    if let Ok(manifest_str) = fetch_plugin_package(client, base).await {
        let manifest: serde_json::Value = serde_json::from_str(&manifest_str)?;
        let name = manifest["name"].as_str().unwrap_or("unknown").to_string();

        let code_url = format!("{base}.mjs");
        let code = client
            .get(&code_url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        code.hash(&mut hasher);
        let hash = format!("{:x}", hasher.finish());

        Ok((name, manifest_str, code, hash))
    } else {
        // Fallback: direct URL fetch (e.g. raw .mjs link or package.json link)
        let fetch_url = if url.ends_with("package.json") {
            url.to_string()
        } else if !url.ends_with(".mjs") {
            format!("{base}.mjs")
        } else {
            url.to_string()
        };

        let code = client
            .get(&fetch_url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;

        let name = base
            .rsplit_once('/')
            .map(|(_, n)| n)
            .unwrap_or("plugin")
            .to_string();
        let manifest = serde_json::json!({"name": &name, "main": &fetch_url}).to_string();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        code.hash(&mut hasher);
        let hash = format!("{:x}", hasher.finish());

        Ok((name, manifest, code, hash))
    }
}

// --- Hamburger menu ---

/// Build the JavaScript snippet that shows the About TidaLunar overlay.
fn about_dialog_js() -> String {
    use base64::Engine;
    let logo_b64 =
        base64::engine::general_purpose::STANDARD.encode(include_bytes!("../tidaluna.png"));
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"(function(){{
if(document.getElementById('tl-about'))return;
var o=document.createElement('div');o.id='tl-about';
o.style.cssText='position:fixed;inset:0;background:rgba(0,0,0,0.6);backdrop-filter:blur(4px);z-index:99999;display:flex;align-items:center;justify-content:center;';
o.innerHTML='<div style="background:#1a1a2e;border-radius:12px;padding:32px;max-width:380px;width:90%;color:#fff;font-family:-apple-system,BlinkMacSystemFont,Segoe UI,Roboto,sans-serif;position:relative;box-shadow:0 20px 60px rgba(0,0,0,0.5);">\
<button id="tl-about-close" style="position:absolute;top:12px;right:16px;background:none;border:none;color:#666;font-size:22px;cursor:pointer;padding:4px 8px;line-height:1;">&times;</button>\
<div style="text-align:center;margin-bottom:20px;">\
<img src="data:image/png;base64,{logo}" style="width:64px;height:64px;margin-bottom:12px;border-radius:12px;">\
<h1 style="margin:0;font-size:20px;font-weight:600;letter-spacing:0.5px;">TidaLunar</h1>\
<p style="margin:4px 0 0;color:#888;font-size:13px;">v{ver} &middot; alpha</p>\
</div>\
<p style="text-align:center;color:#aaa;font-size:13px;margin:0 0 12px;">A native TIDAL client built with Rust</p>\
<div style="text-align:center;margin-bottom:20px;font-size:13px;"><a href="https://github.com/Inrixia/TidaLuna" target="_blank" style="color:#5b8def;text-decoration:none;">GitHub</a> &middot; <a href="https://discord.gg/jK3uHrJGx4" target="_blank" style="color:#7289da;text-decoration:none;">Discord</a></div>\
<hr style="border:none;border-top:1px solid #333;margin:0 0 16px;">\
<p style="color:#666;font-size:11px;text-transform:uppercase;letter-spacing:1px;margin:0 0 10px;">Powered by</p>\
<div style="columns:2;column-gap:16px;color:#999;font-size:12px;line-height:2;">\
<a href="https://github.com/tauri-apps/cef-rs" target="_blank" style="display:block;color:#999;text-decoration:none;">CEF (Chromium)</a>\
<a href="https://github.com/pdeljanov/Symphonia" target="_blank" style="display:block;color:#999;text-decoration:none;">symphonia</a>\
<a href="https://github.com/RustAudio/cpal" target="_blank" style="display:block;color:#999;text-decoration:none;">cpal (Cross-Platform Audio)</a>\
<a href="https://github.com/HEnquist/rubato" target="_blank" style="display:block;color:#999;text-decoration:none;">rubato</a>\
</div>\
<hr style="border:none;border-top:1px solid #333;margin:16px 0;">\
<p style="text-align:center;color:#555;font-size:10px;margin:0;line-height:1.6;">This project is not affiliated with, endorsed by, or connected to TIDAL or its parent companies. TIDAL is a registered trademark of its respective owners.</p>\
</div>';
document.body.appendChild(o);
o.querySelectorAll('a[href]').forEach(function(a){{a.addEventListener('click',function(e){{e.preventDefault();window.cefQuery({{request:JSON.stringify({{channel:'window.open_url',args:[a.href]}}),onSuccess:function(){{}},onFailure:function(){{}}}});}});}});
o.addEventListener('click',function(e){{if(e.target===o)o.remove();}});
document.getElementById('tl-about-close').addEventListener('click',function(){{o.remove();}});
document.addEventListener('keydown',function h(e){{if(e.key==='Escape'){{o.remove();document.removeEventListener('keydown',h);}}}});
}})()"#,
        logo = logo_b64,
        ver = version
    )
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuCommand {
    Settings = 1,
    DevTools = 2,
    About = 3,
    Logout = 4,
    Exit = 5,
    ClearCache = 6,
    OpenData = 7,

    PlayPause = 10,
    Next = 11,
    Prev = 12,
    Stop = 13,
}

impl TryFrom<i32> for MenuCommand {
    type Error = i32;
    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Settings),
            2 => Ok(Self::DevTools),
            3 => Ok(Self::About),
            4 => Ok(Self::Logout),
            5 => Ok(Self::Exit),
            6 => Ok(Self::ClearCache),
            7 => Ok(Self::OpenData),

            10 => Ok(Self::PlayPause),
            11 => Ok(Self::Next),
            12 => Ok(Self::Prev),
            13 => Ok(Self::Stop),
            other => Err(other),
        }
    }
}

wrap_menu_model_delegate! {
    struct HamburgerMenuDelegate {
        _p: u8,
    }
    impl MenuModelDelegate {
        fn execute_command(
            &self,
            _menu_model: Option<&mut MenuModel>,
            command_id: ::std::os::raw::c_int,
            _event_flags: EventFlags,
        ) {
            let Ok(cmd) = MenuCommand::try_from(command_id) else {
                return;
            };
            match cmd {

                MenuCommand::Settings => {
                    eval_js("window.__TL_NAVIGATE__?.('/settings')");
                }
                MenuCommand::DevTools => {
                    toggle_devtools();
                }
                MenuCommand::About => {
                    eval_js(&about_dialog_js());
                }
                MenuCommand::Logout => {
                    eval_js("window.location.href = '/logout';");
                }
                MenuCommand::ClearCache => {
                    if let Ok(mut cache) = state::AUDIO_CACHE.lock()
                        && let Err(e) = cache.clear()
                    {
                        crate::vprintln!("[CACHE]  Clear failed: {e}");
                    }
                }
                MenuCommand::OpenData => {
                    open_in_os(state::cache_data_dir());
                }
                MenuCommand::PlayPause => {
                    eval_js("window.__TL_PLAY_PAUSE__?.()");
                }
                MenuCommand::Next => {
                    eval_js("window.__TIDAL_PLAYBACK_DELEGATE__?.playNext?.();");
                }
                MenuCommand::Prev => {
                    eval_js("window.__TIDAL_PLAYBACK_DELEGATE__?.playPrevious?.();");
                }
                MenuCommand::Stop => {
                    with_state(|state| {
                        let _ = state.player.stop();
                    });
                }
                MenuCommand::Exit => {
                    with_state(|state| {
                        if let Some(window) = get_cef_window(state) {
                            window.close();
                        } else {
                            quit_message_loop();
                        }
                    });
                }
            }
        }
    }
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
        "window.devtools" => {
            toggle_devtools();
        }
        "menu.clicked" => {
            let x = msg.args.first().and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let y = msg.args.get(1).and_then(|v| v.as_i64()).unwrap_or(0) as i32;

            with_state(|state| {
                if let Some(window) = get_cef_window(state) {
                    let mut delegate = HamburgerMenuDelegate::new(0);
                    if let Some(mut menu) = menu_model_create(Some(&mut delegate)) {
                        menu.add_item(
                            MenuCommand::PlayPause as i32,
                            Some(&CefString::from("Play / Pause")),
                        );
                        menu.add_item(MenuCommand::Next as i32, Some(&CefString::from("Next")));
                        menu.add_item(MenuCommand::Prev as i32, Some(&CefString::from("Previous")));
                        menu.add_item(MenuCommand::Stop as i32, Some(&CefString::from("Stop")));
                        menu.add_separator();
                        menu.add_item(
                            MenuCommand::Settings as i32,
                            Some(&CefString::from("Settings")),
                        );

                        let cache_label = if let Ok(cache) = state::AUDIO_CACHE.lock() {
                            let mb = cache.total_size() as f64 / (1024.0 * 1024.0);
                            format!("Clear Cache ({mb:.0} MB)")
                        } else {
                            "Clear Cache".to_string()
                        };
                        menu.add_item(
                            MenuCommand::ClearCache as i32,
                            Some(&CefString::from(cache_label.as_str())),
                        );
                        menu.add_item(
                            MenuCommand::OpenData as i32,
                            Some(&CefString::from("Open Data Folder")),
                        );
                        menu.add_item(
                            MenuCommand::DevTools as i32,
                            Some(&CefString::from("DevTools (F12)")),
                        );
                        menu.add_separator();
                        menu.add_item(
                            MenuCommand::About as i32,
                            Some(&CefString::from("About TidaLunar")),
                        );
                        menu.add_separator();
                        menu.add_item(
                            MenuCommand::Logout as i32,
                            Some(&CefString::from("Log Out")),
                        );
                        menu.add_item(MenuCommand::Exit as i32, Some(&CefString::from("Exit")));

                        let client = window.client_area_bounds_in_screen();
                        let screen_point = Point {
                            x: client.x + x,
                            y: client.y + y,
                        };
                        window.show_menu(
                            Some(&mut menu),
                            Some(&screen_point),
                            MenuAnchorPosition::TOPLEFT,
                        );
                    }
                }
            });
        }
        "window.drag" => {
            // Drag is handled by CSS -webkit-app-region: drag
            // + DragHandler forwarding to Window::set_draggable_regions.
        }
        "window.open_url" => {
            if let Some(url) = msg.args.first().and_then(|v| v.as_str()) {
                open_in_os(url);
            }
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
            } else {
                crate::vprintln!(
                    "[BRIDGE] flush DROPPED — {}",
                    if state.browser.is_none() {
                        "no browser"
                    } else {
                        "no frame"
                    }
                );
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
                crate::vprintln!("[BRIDGE] StateChange: \"{}\" seq={}", st.as_str(), seq);

                // SDK contract: after "completed", auto-load the preloaded next track.
                // The webapp does NOT send player.load for the next track — it expects
                // the native player to transition internally.
                if st == PlaybackState::Completed {
                    let player = state.player.clone();
                    state.rt_handle.spawn(async move {
                        if let Some(next) = preload::take_next_track().await {
                            crate::vprintln!("[AUTO]   Loading preloaded next track");
                            if let Err(e) = player.load_and_play(next.url, next.format, next.key) {
                                eprintln!("[AUTO]   Failed to load next track: {e}");
                            }
                        } else {
                            crate::vprintln!("[AUTO]   No preloaded next track");
                        }
                    });
                }

                if let Some(ref mut mc) = state.media_controls {
                    mc.set_playback(st);
                }

                #[cfg(target_os = "windows")]
                if let Some(ref tb) = state.thumbbar {
                    tb.set_playing(matches!(
                        st,
                        PlaybackState::Active | PlaybackState::Seeking | PlaybackState::Idle
                    ));
                }

                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::state(st.as_str(), seq));
            }
            PlayerEvent::Duration(duration, seq) => {
                state.media_duration = Some(duration);

                // Update SMTC metadata with the now-known duration.
                if let Some(ref mut mc) = state.media_controls
                    && let Some(ref meta) = *crate::state::CURRENT_METADATA
                        .lock()
                        .expect("CURRENT_METADATA lock poisoned")
                {
                    mc.set_metadata(&meta.title, &meta.artist, Some(duration));
                }

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
                bytes,
            } => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::media_format(
                        codec,
                        sample_rate,
                        bit_depth,
                        channels,
                        bytes,
                    ));
            }
            PlayerEvent::Version(v) => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::version(v));
            }
            PlayerEvent::DeviceError(kind) => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::device_error(kind.as_str()));
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
        // Try to parse the message to check for plugin invoke channels.
        // Plugin channels with an id use the callback for the response;
        // everything else is fire-and-forget (respond "ok" immediately).
        if let Ok(msg) = serde_json::from_str::<IpcMessage>(request)
            && (msg.channel.starts_with("plugin.")
                || msg.channel.starts_with("proxy.")
                || msg.channel.starts_with("jsrt.")
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
            0
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
            // Open external HTTP(S) links in the system browser, cancel all popups.
            if let Some(url) = target_url {
                let url_str = url.to_string();
                if (url_str.starts_with("http://") || url_str.starts_with("https://"))
                    && !url_str.contains("desktop.tidal.com")
                {
                    open_in_os(&url_str);
                }
            }
            1 // Cancel popup — never open a CEF popup window
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

// --- Load Handler (inject initialization scripts) ---

#[derive(Clone, Copy, PartialEq)]
enum PageState {
    Initial, // first load not completed yet
    App,     // on a non-login page (desktop.tidal.com/*)
    Login,   // on a login page (desktop.tidal.com/login/*)
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
            // Inject scripts when the main frame finishes loading on
            // desktop.tidal.com.  Skip sub-frames, about:blank,
            // login.tidal.com, devtools, etc.
            // The bundle is wrapped in a JS guard so that double-fires of
            // on_loading_state_change for the same page are harmless.
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

                // After login redirect (login/auth → /), force a full
                // reload so the webapp starts fresh and calls Player().
                // Stop the player first to discard any state accumulated
                // during the login pages.
                if prev == PageState::Login && !is_login {
                    self.page_state.set(PageState::Initial);
                    with_state(|state| {
                        let _ = state.player.stop();
                        // Discard any bridge events queued during login pages
                        // so they don't leak into the fresh post-reload context.
                        state.pending_player_events.clear();
                        state.pending_time_update = None;
                    });
                    crate::vprintln!("[LOAD]   Post-login redirect detected, reloading");
                    browser.reload();
                    return;
                }

                // Stop playback only when navigating TO login (logout).
                // SPA internal navigation (e.g. /tracks, /album/xxx) must
                // NOT interrupt playback — TIDAL uses history.pushState().
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
                // Guard: skip bundle if already injected in this JS context
                // (handles double-fire of on_loading_state_change).
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
            WM_COMMAND => {
                // Thumbnail toolbar button clicks (ITaskbarList3)
                let id = (wparam & 0xFFFF) as u32;
                match id {
                    0 => crate::eval_js("window.__TIDAL_PLAYBACK_DELEGATE__?.playPrevious?.()"),
                    1 => crate::eval_js("window.__TL_PLAY_PAUSE__?.()"),
                    2 => crate::eval_js("window.__TIDAL_PLAYBACK_DELEGATE__?.playNext?.()"),
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

            {
                #[cfg(target_os = "windows")]
                let hwnd = Some(window.window_handle().0 as *mut std::ffi::c_void);
                #[cfg(not(target_os = "windows"))]
                let hwnd = None;

                if let Some(mc) = media_controls::OsMediaControls::new(hwnd) {
                    with_state(|state| {
                        state.media_controls = Some(mc);
                    });
                }
            }

            window.show();

            // Thumbnail toolbar buttons (Previous / Play-Pause / Next) in the
            // Windows taskbar.  Must be created after the window is shown so
            // the shell has registered the HWND.
            #[cfg(target_os = "windows")]
            {
                let hwnd = window.window_handle().0 as windows_sys::Win32::Foundation::HWND;
                if let Some(tb) = thumbbar::ThumbBar::new(hwnd) {
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

/// Command-line switches passed to the browser process.
/// CEF propagates relevant ones to subprocesses automatically.
/// TIDAL needs: DOM, CSS, JS, fetch/XHR, cookies, localStorage,
/// Canvas 2D, WebGL (cover art/transitions). Everything else is dead weight.
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

/// Chromium features to disable via `--disable-features=X,Y,Z`.
/// Uses `append_switch` (not `append_switch_with_value`) to work around
/// a crash on Windows (ACCESS_VIOLATION in add_child_view).
const CEF_DISABLED_FEATURES: &[&str] = &[
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
    // Media keys — souvlaki handles hardware media keys via SMTC;
    // disable CEF's interception to avoid conflicts.
    "HardwareMediaKeyHandling",
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
    "BackForwardCache",
    "SpareRendererForSitePerProcess",
    "GlobalMediaControls",
    "MediaRouter",
    "OptimizationHints",
    "CalculateNativeWinOcclusion",
];

const CEF_ENABLED_FEATURES: &[&str] = &["PartitionAllocMemoryReclaimer"];

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
            // Disable password manager and autofill via Chromium preferences.
            // Command-line switches alone are insufficient in modern CEF.
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

            // --- Create browser view and window ---
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

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "windows")]
    unsafe {
        use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
        let app_id: Vec<u16> = "com.tidalunar.app\0".encode_utf16().collect();
        SetCurrentProcessExplicitAppUserModelID(app_id.as_ptr());

        // Register a display name for the AppUserModelID so that Windows
        // shows "TidaLunar" instead of "Unknown app" in Quick Settings.
        use windows_sys::Win32::System::Registry::{
            HKEY_CURRENT_USER, KEY_WRITE, REG_SZ, RegCreateKeyExW, RegSetValueExW,
        };
        let subkey: Vec<u16> = "Software\\Classes\\AppUserModelId\\com.tidalunar.app\0"
            .encode_utf16()
            .collect();
        let mut hkey = core::ptr::null_mut();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            core::ptr::null(),
            0,
            KEY_WRITE,
            core::ptr::null(),
            &mut hkey,
            core::ptr::null_mut(),
        ) == 0
        {
            let name: Vec<u16> = "DisplayName\0".encode_utf16().collect();
            let value: Vec<u16> = "TidaLunar\0".encode_utf16().collect();
            let _ = RegSetValueExW(
                hkey,
                name.as_ptr(),
                0,
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            );
            windows_sys::Win32::System::Registry::RegCloseKey(hkey);
        }
    }

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
    let data_dir = state::cache_data_dir();
    let app_settings =
        settings::Settings::open(&data_dir).expect("Failed to open settings database");
    let plugin_store =
        plugins::PluginStore::open(&data_dir).expect("Failed to open plugin store database");

    // --- Initialize QuickJS plugin runtime ---
    let plugin_runtime = match js_runtime::plugin_runtime::PluginRuntime::new() {
        Ok(rt) => {
            crate::vprintln!("[JS_RUNTIME] QuickJS plugin runtime initialized");
            Some(rt)
        }
        Err(e) => {
            eprintln!("[JS_RUNTIME] Failed to init QuickJS runtime: {e}");
            None
        }
    };

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
        media_controls: None,
        media_duration: None,
        plugin_store,
        pending_ipc_callbacks: HashMap::new(),
        plugin_runtime,
        #[cfg(target_os = "windows")]
        thumbbar: None,
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
        chrome_app_icon_id: 101,
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
