use crate::app_state::{IpcMessage, eval_js, with_state};

pub(crate) fn handle_jsrt_fire_and_forget(msg: &IpcMessage) {
    match msg.channel.as_str() {
        "jsrt.set_token" => {
            let token = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            with_state(|state| {
                state.captured_token = token.to_string();
            });
            crate::vprintln!("[PLUGIN] Token captured ({} chars)", token.len());
        }
        "jsrt.enable_plugin" => {
            let url = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            if url.is_empty() {
                return;
            }
            let url_owned = url.to_owned();
            let code = crate::state::db().call(move |pc, _| {
                crate::plugins::store::get_code(pc, &url_owned)
            });
            if let Some(code) = code {
                match crate::plugins::PluginManager::transpile_and_wrap(url, &code) {
                    Ok(js) => {
                        with_state(|state| state.plugin_manager.mark_loaded(url));
                        eval_js(&js);
                    }
                    Err(e) => {
                        crate::vprintln!("[PLUGIN] Failed to prepare '{}': {}", url, e);
                    }
                }
            }
        }
        "jsrt.disable_plugin" => {
            let url = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            if url.is_empty() {
                return;
            }
            crate::vprintln!("[PLUGIN] disable_plugin requested for '{}'", url);
            let cleanup_js = with_state(|state| {
                crate::vprintln!(
                    "[PLUGIN] is_loaded('{}') = {}",
                    url,
                    state.plugin_manager.is_loaded(url)
                );
                state.plugin_manager.unload_plugin(url)
            })
            .flatten();
            if let Some(ref js) = cleanup_js {
                crate::vprintln!(
                    "[PLUGIN] Unloading '{}' ({} bytes cleanup JS)",
                    url,
                    js.len()
                );
                eval_js(js);
            } else {
                crate::vprintln!("[PLUGIN] No cleanup needed for '{}' (not loaded)", url);
            }
        }
        "jsrt.load_plugins" => {
            let plugin_code = crate::state::db().call(|pc, _| {
                crate::plugins::store::collect_enabled_code(pc)
            });
            let mut prepared = Vec::new();
            for (url, name, code) in &plugin_code {
                match crate::plugins::PluginManager::transpile_and_wrap(url, code) {
                    Ok(js) => {
                        crate::vprintln!("[PLUGIN] Prepared '{}' ({} bytes)", name, js.len());
                        prepared.push((url.as_str(), js));
                    }
                    Err(e) => {
                        crate::vprintln!("[PLUGIN] Failed to prepare '{}': {e}", name);
                    }
                }
            }
            crate::vprintln!("[PLUGIN] Loading {} plugins into CEF", prepared.len());
            with_state(|state| {
                for (url, _) in &prepared {
                    state.plugin_manager.mark_loaded(url);
                }
            });
            for (url, js) in &prepared {
                crate::vprintln!("[PLUGIN] Injecting '{}' ({} bytes)", url, js.len());
                eval_js(js);
            }
        }
        // Channels no longer needed (were for QuickJS) — silently ignore
        "jsrt.tick"
        | "jsrt.state_sync"
        | "jsrt.redux_action"
        | "jsrt.event"
        | "jsrt.set_product_id"
        | "jsrt.dom_query_result"
        | "jsrt.dom_query_result_all"
        | "jsrt.dom_raw_query_result"
        | "jsrt.dom_observe"
        | "jsrt.dom_event"
        | "jsrt.dom_mutation"
        | "jsrt.dom_ready" => {}
        _ => {
            crate::vprintln!("[JSRT] Unknown fire-and-forget channel: {}", msg.channel);
        }
    }
}
