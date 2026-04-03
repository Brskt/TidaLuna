mod jsrt;
mod native;
mod plugin_ipc;
mod proxy;

pub(crate) use jsrt::handle_jsrt_fire_and_forget;

use crate::app_state::{IpcCallback, IpcMessage, with_state};
use std::sync::atomic::{AtomicBool, Ordering};

/// Scrub the PKCE codeVerifier from nativeInterface after successful auth.
/// Called from both jsrt.set_token (primary path) and proxy.rs oauth2/token (fallback).
/// Best-effort: eval_js may fail if no renderer frame is available.
static PKCE_SCRUBBED: AtomicBool = AtomicBool::new(false);
/// Re-arm the scrub latch so the next login cycle triggers a fresh scrub.
/// Called from session_clear and session_hard_reset.
pub(super) fn reset_pkce_scrub() {
    PKCE_SCRUBBED.store(false, Ordering::SeqCst);
}

pub(crate) fn scrub_pkce_verifier() {
    if PKCE_SCRUBBED.swap(true, Ordering::SeqCst) {
        return; // already scrubbed
    }
    crate::app_state::eval_js(
        "try{delete window.nativeInterface.credentials.codeVerifier}catch(e){}",
    );
    crate::vprintln!("[AUTH]   PKCE codeVerifier scrub requested");
}

fn take_ipc_callback(id: &str) -> Option<IpcCallback> {
    with_state(|state| state.pending_ipc_callbacks.remove(id)).flatten()
}

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

pub(crate) fn handle_plugin_ipc(msg: IpcMessage, callback: IpcCallback) {
    match msg.channel.as_str() {
        "plugin.fetch" => {
            plugin_ipc::handle_plugin_fetch(&msg, callback);
        }
        "tidal.fetch" => {
            plugin_ipc::handle_tidal_fetch(&msg, callback);
        }
        "player.parse_dash" => {
            let xml = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            match crate::player::dash::parse_dash_mpd(xml) {
                Ok(manifest) => {
                    let json = serde_json::to_string(&manifest).unwrap_or_else(|_| "null".into());
                    ipc_callback_ok(&callback, &json);
                }
                Err(e) => ipc_callback_err(&callback, &format!("{e:#}")),
            }
        }
        "proxy.fetch" => {
            proxy::handle_proxy_fetch_dispatch(&msg, callback);
        }
        "proxy.head" => {
            proxy::handle_proxy_head_dispatch(&msg, callback);
        }
        "__Luna.registerNative" => {
            native::handle_register_native(&msg, callback);
        }
        ch if ch.starts_with("__LunaNative.") => {
            native::handle_native_call(&msg, callback);
        }
        "plugin.fetch_package" => {
            plugin_ipc::handle_plugin_fetch_package(&msg, callback);
        }
        "plugin.install" => {
            plugin_ipc::handle_plugin_install(&msg, callback);
        }
        "plugin.check_hash" => {
            plugin_ipc::handle_plugin_check_hash(&msg, callback);
        }
        "plugin.enable" => {
            plugin_ipc::handle_plugin_enable(&msg, callback);
        }
        "plugin.disable" => {
            plugin_ipc::handle_plugin_disable(&msg, callback);
        }
        "jsrt.load_plugins" => {
            plugin_ipc::handle_jsrt_load_plugins(callback);
        }
        _ => {
            plugin_ipc::handle_plugin_db(msg, callback);
        }
    }
}
