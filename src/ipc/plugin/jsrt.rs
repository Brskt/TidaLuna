use crate::app_state::{IpcMessage, eval_js, with_state};
use cef::*;

// Soft clear: token only. TIDAL calls this during "not logged in" flow —
// must not destroy cookies/localStorage/sessionStorage.

fn handle_session_clear() {
    crate::vprintln!("[AUTH]   session_clear received");

    with_state(|state| {
        state.captured_token.clear();
    });
    crate::vprintln!("[AUTH]   Cleared captured token");

    crate::app_state::emit_ipc_event("jsrt.session_cleared");
    crate::vprintln!("[AUTH]   Notified frontend: session_cleared");
}

// --- Session hard reset ---
// Aggressive cleanup: cookies, storage, token. Used for debug/manual logout only.

static HARD_RESET_IN_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn handle_session_hard_reset() {
    if HARD_RESET_IN_PROGRESS.swap(true, std::sync::atomic::Ordering::SeqCst) {
        crate::vprintln!("[AUTH]   hard_reset already in progress, ignoring");
        return;
    }
    crate::vprintln!("[AUTH]   hard_reset received");

    with_state(|state| {
        state.captured_token.clear();
    });
    crate::vprintln!("[AUTH]   Cleared captured token");

    if let Some(cm) = cef::cookie_manager_get_global_manager(None) {
        let empty = cef::CefString::from("");
        let mut cb = HardResetCookiesCallback::new(0);
        let ok = cm.delete_cookies(Some(&empty), Some(&empty), Some(&mut cb));
        if ok != 0 {
            crate::vprintln!("[AUTH]   delete_cookies started (async)");
        } else {
            crate::vprintln!("[AUTH]   delete_cookies failed synchronously");
            hard_reset_finalize();
        }
    } else {
        crate::vprintln!("[AUTH]   Cookie manager not available");
        hard_reset_finalize();
    }
}

fn hard_reset_finalize() {
    eval_js("try { localStorage.clear(); sessionStorage.clear(); } catch(e) {}");
    crate::vprintln!("[AUTH]   Cleared web storage");

    crate::app_state::emit_ipc_event("jsrt.session_hard_reset_done");
    crate::vprintln!("[AUTH]   hard_reset complete");

    HARD_RESET_IN_PROGRESS.store(false, std::sync::atomic::Ordering::SeqCst);
}

cef::wrap_delete_cookies_callback! {
    struct HardResetCookiesCallback {
        _p: u8,
    }
    impl DeleteCookiesCallback {
        fn on_complete(&self, num_deleted: ::std::os::raw::c_int) {
            crate::vprintln!("[AUTH]   delete_cookies complete ({} deleted)", num_deleted);
            if let Some(cm) = cef::cookie_manager_get_global_manager(None) {
                let mut cb = HardResetFlushCallback::new(0);
                let ok = cm.flush_store(Some(&mut cb));
                if ok != 0 {
                    crate::vprintln!("[AUTH]   flush_store started (async)");
                } else {
                    crate::vprintln!("[AUTH]   flush_store failed synchronously");
                    hard_reset_finalize();
                }
            } else {
                hard_reset_finalize();
            }
        }
    }
}

cef::wrap_completion_callback! {
    struct HardResetFlushCallback {
        _p: u8,
    }
    impl CompletionCallback {
        fn on_complete(&self) {
            crate::vprintln!("[AUTH]   flush_store complete");
            hard_reset_finalize();
        }
    }
}

pub(crate) fn handle_jsrt_fire_and_forget(msg: &IpcMessage) {
    match msg.channel.as_str() {
        "jsrt.set_token" => {
            let token = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            with_state(|state| {
                state.captured_token = token.to_string();
            });
            crate::vprintln!("[PLUGIN] Token captured ({} chars)", token.len());
        }
        "jsrt.session_clear" => {
            handle_session_clear();
        }
        "jsrt.session_hard_reset" => {
            handle_session_hard_reset();
        }
        "jsrt.enable_plugin" => {
            let url = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            if url.is_empty() {
                return;
            }
            let url_owned = url.to_owned();
            let code = crate::state::db()
                .call_plugins(move |pc| crate::plugins::store::get_code(pc, &url_owned));
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
            let plugins =
                crate::state::db().call_plugins(crate::plugins::store::collect_enabled_code);
            let mut prepared = Vec::new();
            for p in &plugins {
                match crate::plugins::PluginManager::transpile_and_wrap(&p.url, &p.code) {
                    Ok(js) => {
                        crate::vprintln!("[PLUGIN] Prepared '{}' ({} bytes)", p.name, js.len());
                        prepared.push((p.url.as_str(), js));
                    }
                    Err(e) => {
                        crate::vprintln!("[PLUGIN] Failed to prepare '{}': {e}", p.name);
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
        _ => {
            crate::vprintln!("[JSRT] Unknown fire-and-forget channel: {}", msg.channel);
        }
    }
}
