mod jsrt;
mod native;
mod plugin_ipc;
mod proxy;

pub(crate) use jsrt::handle_jsrt_fire_and_forget;

use crate::app_state::{IpcCallback, IpcMessage, with_state};

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
        _ => {
            plugin_ipc::handle_plugin_db(msg, callback);
        }
    }
}
