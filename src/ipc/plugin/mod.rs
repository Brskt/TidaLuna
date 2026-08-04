mod download;
mod jsrt;
mod lib_native;
mod native;
mod plugin_ipc;
mod proxy;

pub(crate) use jsrt::JS_PURGE_SDK_BLOB;
pub(crate) use jsrt::handle_jsrt_fire_and_forget;
pub(crate) use jsrt::open_plugin_gate;
// Token-scrub for plugin-facing response bodies; reused by the native egress path.
pub(crate) use proxy::scrub_real_tokens;

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

fn take_ipc_callback(query_id: i64) -> Option<IpcCallback> {
    with_state(|state| state.pending_ipc_callbacks.remove(&query_id)).flatten()
}

/// Called from `IpcQueryHandler::on_query_canceled`. The in-flight task then finds `None`
/// and returns instead of touching a callback the router has retired.
pub(crate) fn drop_ipc_callback(query_id: i64) {
    with_state(|state| state.pending_ipc_callbacks.remove(&query_id));
}

pub(crate) fn ipc_callback_ok(cb: &IpcCallback, result: &str) {
    cb.lock()
        .unwrap_or_else(|e| e.into_inner())
        .success_str(result);
}

pub(crate) fn ipc_callback_err(cb: &IpcCallback, code: i32, msg: &str) {
    cb.lock()
        .unwrap_or_else(|e| e.into_inner())
        .failure(code, msg);
}

pub(crate) fn handle_plugin_ipc(msg: IpcMessage, query_id: i64, callback: IpcCallback) {
    // Resolved once, before dispatch: a handler resolving again could read a capability the gate
    // accepted and eviction has since dropped, turning an allowed call unattributed after the fact.
    let caller = crate::ipc::caller::Caller::resolve(&msg);
    if is_plugin_attributed(&msg.channel)
        && let Err((code, why)) = caller.require_plugin()
    {
        // Gated: the caller gets the refusal as its reply; the log is not the only channel, and
        // an ungated write would let a refused-call loop drive the disk from the renderer.
        crate::vprintln!("[IPC] Refused {}: {why}", msg.channel);
        ipc_callback_err(&callback, code, why);
        return;
    }
    match msg.channel.as_str() {
        "plugin.fetch" => {
            plugin_ipc::handle_plugin_fetch(&msg, &caller, query_id, callback);
        }
        "tidal.fetch" => {
            plugin_ipc::handle_tidal_fetch(&msg, query_id, callback);
        }
        "player.parse_dash" => {
            let xml = msg.arg(0);
            match crate::player::dash::parse_dash_mpd(xml) {
                Ok(manifest) => {
                    let json = serde_json::to_string(&manifest).unwrap_or_else(|_| "null".into());
                    ipc_callback_ok(&callback, &json);
                }
                Err(e) => ipc_callback_err(&callback, 400, &format!("{e:#}")),
            }
        }
        "plugin.download" => {
            download::handle_download(&msg, callback);
        }
        "proxy.fetch" => {
            proxy::handle_proxy_fetch_dispatch(&msg, query_id, callback);
        }
        "proxy.head" => {
            proxy::handle_proxy_head_dispatch(&msg, query_id, callback);
        }
        "__Luna.registerNative" => {
            native::handle_register_native(&msg, callback);
        }
        "__Luna.clipboardWriteText" => {
            lib_native::handle_clipboard_write_text(&msg, callback);
        }
        "__Luna.openExternal" => {
            lib_native::handle_open_external(&msg, callback);
        }
        "__Luna.sendToRender" => {
            lib_native::handle_send_to_render(&msg, callback);
        }
        "__Luna.showMessageBox" => {
            lib_native::handle_show_message_box(&msg, callback);
        }
        "__Luna.showErrorBox" => {
            lib_native::handle_show_error_box(&msg, callback);
        }
        "__Luna.showOpenDialog" => {
            lib_native::handle_show_open_dialog(&msg, callback);
        }
        "__Luna.showSaveDialog" => {
            lib_native::handle_show_save_dialog(&msg, callback);
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
            plugin_ipc::handle_plugin_check_hash(&msg, query_id, callback);
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
        "updater.check" => {
            crate::updater::handle_updater_check(callback);
        }
        "updater.download" => {
            crate::updater::handle_updater_download(&msg, callback);
        }
        "updater.status" => {
            crate::updater::handle_updater_status(callback);
        }
        _ => {
            plugin_ipc::handle_plugin_db(msg, &caller, callback);
        }
    }
}

/// Channels whose handler acts on the caller's identity: serving one unattributed would mean
/// acting on a name the caller chose. Only channels the wrapper's own shims reach can be listed, since
/// those carry the capability. Anything through the shared `@luna/lib` arrives unattributed and is
/// refused, `plugin.download` being the case in point. Matched exactly, not by prefix:
/// `plugin.fetch_package` installs a plugin before one exists to hold a capability.
fn is_plugin_attributed(channel: &str) -> bool {
    channel.starts_with("plugin.storage.") || channel == "plugin.fetch"
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/mod.rs"]
mod tests;
