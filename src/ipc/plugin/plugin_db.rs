use super::uninstall::handle_plugin_uninstall;
use super::{ipc_callback_err, ipc_callback_ok};
use crate::app_state::{IpcCallback, IpcMessage};

pub(super) fn handle_plugin_db(
    msg: IpcMessage,
    caller: &crate::ipc::caller::Caller,
    callback: IpcCallback,
) {
    let db = crate::state::db();
    let channel = msg.channel.clone();
    // The storage namespace is the caller's url, never argument 0: that named a namespace with no
    // check the caller owned it. Taken from the gate's resolution rather than resolved a second
    // time, which could answer differently and silently move the write.
    let caller_ns = match caller {
        crate::ipc::caller::Caller::Plugin(url) => url.clone(),
        crate::ipc::caller::Caller::Unattributed => String::new(),
    };

    // Storage rows are keyed by the caller: an unattributed one has no namespace, and `""` is not
    // inert (it is a row every unattributed caller would share). The gate already refuses these
    // channels; this is the floor under it.
    if channel.starts_with("plugin.storage.") && caller_ns.is_empty() {
        ipc_callback_err(&callback, 403, "unattributed call");
        return;
    }

    if channel == "plugin.uninstall" || channel == "plugin.uninstall_all" {
        handle_plugin_uninstall(channel, msg.arg(0).to_string(), callback);
        return;
    }

    // Posted rather than called: this arrives on the CEF UI thread, where waiting on the actor
    // freezes rendering and input for the whole round trip. The actor answers the renderer from
    // its own thread, the callback marshalling back to the UI thread by itself.
    db.post(move |pc, _| {
        // Wrapped in a closure to let the arms below answer with `return Err(...)`: the enclosing
        // closure owes the actor nothing, and a bare return there would abandon the reply the
        // renderer waits on.
        let result: Result<String, String> = (|| match channel.as_str() {
            "plugin.list" => {
                let plugins =
                    crate::plugins::store::list(pc).map_err(|e| format!("DB error: {e}"))?;
                Ok(serde_json::to_string(&plugins).unwrap_or_else(|_| "[]".to_string()))
            }
            "plugin.storage.get" => {
                let ns = caller_ns.as_str();
                let key = msg.arg(1);
                match crate::plugins::store::storage_get(pc, ns, key) {
                    Some(val) => {
                        Ok(serde_json::to_string(&val).unwrap_or_else(|_| "null".to_string()))
                    }
                    None => Ok("null".to_string()),
                }
            }
            "plugin.storage.set" => {
                let ns = caller_ns.as_str();
                let key = msg.arg(1);
                let value = msg.arg(2);
                crate::plugins::store::storage_set(pc, ns, key, value)
                    .map(|()| "true".to_string())
                    .map_err(|e| e.to_string())
            }
            "plugin.storage.del" => {
                let ns = caller_ns.as_str();
                let key = msg.arg(1);
                crate::plugins::store::storage_del(pc, ns, key)
                    .map(|()| "true".to_string())
                    .map_err(|e| e.to_string())
            }
            "plugin.storage.keys" => {
                let ns = caller_ns.as_str();
                let keys = crate::plugins::store::storage_keys(pc, ns);
                Ok(serde_json::to_string(&keys).unwrap_or_else(|_| "[]".to_string()))
            }
            "plugin.cleanup_failed_install" => {
                let url = msg.arg(0);
                // Triple guard: enabled=0 AND ever_dispatched=0
                let (enabled, ever_dispatched): (bool, bool) = pc
                    .query_row(
                        "SELECT enabled, ever_dispatched FROM plugins WHERE url = ?1 AND installed = 1",
                        rusqlite::params![url],
                        |row| Ok((row.get::<_, i32>(0)? != 0, row.get::<_, i32>(1)? != 0)),
                    )
                    .unwrap_or((true, true));
                if enabled {
                    return Err("Cannot cleanup: plugin is currently enabled.".to_string());
                }
                if ever_dispatched {
                    return Err(
                        "Cannot cleanup: plugin code was previously dispatched. Use plugin.uninstall instead."
                            .to_string(),
                    );
                }
                crate::plugins::store::uninstall(pc, url)
                    .map(|()| "true".to_string())
                    .map_err(|e| e.to_string())
            }
            _ => Err(format!("Unknown plugin channel: {}", channel)),
        })();

        match result {
            Ok(json) => ipc_callback_ok(&callback, &json),
            Err(e) => ipc_callback_err(&callback, 500, &e),
        }
    });
}
