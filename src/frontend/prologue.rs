//! The values Rust hands the page scripts, as JSON assignments ahead of files that hold none.
//!
//! Every value goes through `js_value`, `serde_json` and then the U+2028/U+2029 escape: the
//! scripts on disk carry no placeholder a value could break out of, and the host names stay owned
//! by `ui::nav`.

use serde_json::{Value, json};

use crate::app_state::escape_js_line_terminators;
use crate::ui::nav;

/// The one encoder for a value placed in page script. Taking a `Value` keeps a pre-formatted
/// string from reaching the output around it.
fn js_value(value: &Value) -> String {
    escape_js_line_terminators(value.to_string())
}

/// `self.__LUNAR_CONFIG__`, read by the early-runtime scripts in the renderer.
pub(crate) fn renderer_config_js() -> String {
    let config = json!({
        "desktopHost": nav::HOST_DESKTOP,
        "loginHost": nav::HOST_LOGIN,
        "authHost": nav::HOST_AUTH,
        "apiHost": nav::HOST_API,
        "redirectUri": nav::REDIRECT_URI,
        "loginCallbackPath": "/login/auth",
        "authHosts": [nav::HOST_LOGIN, nav::HOST_AUTH],
        "nativeWindowButtons": cfg!(target_os = "macos"),
        "systemFrame": crate::ui::SYSTEM_FRAME,
    });
    format!("self.__LUNAR_CONFIG__={};", js_value(&config))
}

/// What `init_script` publishes as `window.__TIDALUNAR_*__` before the page runs.
pub(crate) struct InitGlobals {
    pub platform: &'static str,
    pub close_to_tray: bool,
    pub auto_check: bool,
    pub update_channel: &'static str,
    pub receiver_always_on: bool,
    pub volume_sync: bool,
    pub asio: bool,
    pub console: bool,
    pub crossfade_enabled: bool,
    pub crossfade_secs: u8,
    pub perf: bool,
    pub window_maximized: bool,
    pub credentials: serde_json::Value,
    pub rewrite_epoch: u64,
}

pub(crate) fn init_globals_js(g: &InitGlobals) -> String {
    let mut js = String::new();
    let globals: [(&str, Value); 13] = [
        ("PLATFORM", json!(g.platform)),
        ("CLOSE_TO_TRAY", json!(g.close_to_tray)),
        ("AUTO_CHECK", json!(g.auto_check)),
        ("UPDATE_CHANNEL", json!(g.update_channel)),
        ("RECEIVER_ALWAYS_ON", json!(g.receiver_always_on)),
        ("VOLUME_SYNC", json!(g.volume_sync)),
        ("ASIO", json!(g.asio)),
        ("CONSOLE", json!(g.console)),
        ("CROSSFADE_ENABLED", json!(g.crossfade_enabled)),
        ("CROSSFADE_SECS", json!(g.crossfade_secs)),
        ("PERF", json!(g.perf)),
        (
            "WINDOW_STATE",
            json!({ "isMaximized": g.window_maximized, "isFullscreen": false }),
        ),
        ("CREDENTIALS", g.credentials.clone()),
    ];
    for (name, value) in &globals {
        js.push_str(&format!(
            "window.__TIDALUNAR_{name}__ = {};\n",
            js_value(value)
        ));
    }
    // A JSON string, not a number: `cache_heal.js` compares it with the one in localStorage.
    let epoch = json!(format!("{:016x}", g.rewrite_epoch));
    js.push_str(&format!("self.__lunaRewriteEpoch={};\n", js_value(&epoch)));
    js
}
