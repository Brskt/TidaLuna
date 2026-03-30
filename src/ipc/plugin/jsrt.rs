use crate::app_state::{IpcMessage, eval_js, with_state};
use cef::*;

// Soft clear: token only. TIDAL calls this during "not logged in" flow —
// must not destroy cookies/localStorage/sessionStorage.

fn handle_session_clear() {
    crate::vprintln!("[AUTH]   session_clear received");

    with_state(|state| {
        state.captured_token.clear();
    });
    super::reset_pkce_scrub();
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
    super::reset_pkce_scrub();
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

/// Multi-pass plugin loading with dependency ordering and startup reconciliation.
/// Used by both the fire-and-forget path and the request-response handler.
pub(super) fn do_load_plugins_inline() {
    let db = crate::state::db();

    // 1. Dedup same-name plugins (legacy/corruption cleanup)
    let deduped = db.call_plugins(crate::plugins::store::dedup_same_name);
    for (url, name) in &deduped {
        crate::vprintln!("[PLUGIN] Startup dedup: removed '{name}' ({url})");
    }

    // 2. Collect all enabled plugins with code + manifest
    let mut remaining: Vec<crate::plugins::store::EnabledPlugin> =
        db.call_plugins(crate::plugins::store::collect_enabled_code);
    let mut loaded_names = std::collections::HashSet::new();
    let mut failed_urls = Vec::new();

    // 3. Multi-pass: load plugins whose deps are satisfied
    let mut dispatched_snapshot: Vec<(String, u64)> = Vec::new(); // (url, load_id) for timeout

    loop {
        let mut progress = false;
        let mut still_remaining = Vec::new();

        for p in remaining {
            match crate::plugins::store::parse_luna_meta(&p.manifest) {
                Err(msg) => {
                    crate::vprintln!("[PLUGIN] Skipping '{}': invalid manifest: {msg}", p.name);
                    failed_urls.push(p.url);
                }
                Ok(meta) => {
                    let deps = meta
                        .as_ref()
                        .map(|m| &m.dependencies[..])
                        .unwrap_or_default();
                    let deps_satisfied = deps.iter().all(|d| loaded_names.contains(&d.name));

                    if deps_satisfied {
                        let (load_id, nonce) =
                            with_state(|state| state.plugin_manager.mark_loading(&p.url))
                                .unwrap_or((0, 0));
                        match crate::plugins::PluginManager::transpile_and_wrap(
                            &p.url, &p.code, load_id, nonce,
                        ) {
                            Ok(js) => {
                                crate::vprintln!(
                                    "[PLUGIN] Prepared '{}' ({} bytes, gen={})",
                                    p.name,
                                    js.len(),
                                    load_id
                                );
                                let dispatched = eval_js(&js);
                                if dispatched {
                                    // Critical: persist ever_dispatched flag
                                    let url_flag = p.url.clone();
                                    let flag_ok = db.call_plugins(move |pc| {
                                        crate::plugins::store::mark_ever_dispatched(pc, &url_flag)
                                    });
                                    if flag_ok.is_ok() {
                                        dispatched_snapshot.push((p.url.clone(), load_id));
                                        loaded_names.insert(p.name);
                                        progress = true;
                                    } else {
                                        // Flag failed — revert to avoid inconsistent state
                                        crate::vprintln!(
                                            "[PLUGIN] Failed to persist ever_dispatched for '{}' — reverting",
                                            p.name
                                        );
                                        let cleanup =
                                            crate::plugins::PluginManager::generate_unload_js(
                                                &p.url,
                                            );
                                        eval_js(&cleanup);
                                        with_state(|state| {
                                            state.plugin_manager.mark_unloaded(&p.url)
                                        });
                                        failed_urls.push(p.url);
                                    }
                                } else {
                                    crate::vprintln!("[PLUGIN] No renderer frame for '{}'", p.name);
                                    with_state(|state| state.plugin_manager.mark_unloaded(&p.url));
                                    failed_urls.push(p.url);
                                }
                            }
                            Err(e) => {
                                crate::vprintln!("[PLUGIN] Failed to prepare '{}': {e}", p.name);
                                with_state(|state| state.plugin_manager.mark_unloaded(&p.url));
                                failed_urls.push(p.url);
                            }
                        }
                    } else {
                        still_remaining.push(p);
                    }
                }
            }
        }

        remaining = still_remaining;
        if !progress || remaining.is_empty() {
            break;
        }
    }

    // 4. Log + reconcile: plugins with unresolved deps
    for p in &remaining {
        crate::vprintln!("[PLUGIN] Cannot load '{}': unresolved dependencies", p.name);
        failed_urls.push(p.url.clone());
    }

    // 5. Reconciliation: auto-disable plugins that couldn't load
    if !failed_urls.is_empty() {
        let urls = failed_urls.clone();
        db.call_plugins(move |pc| {
            for url in &urls {
                if let Err(e) = crate::plugins::store::disable(pc, url) {
                    crate::vprintln!("[PLUGIN] Failed to auto-disable '{url}': {e}");
                } else {
                    crate::vprintln!("[PLUGIN] Auto-disabled '{url}': failed to load at startup");
                }
            }
        });
    }

    crate::vprintln!(
        "[PLUGIN] Startup complete: {} loaded, {} failed",
        loaded_names.len(),
        failed_urls.len()
    );

    // 6. Startup timeout: check dispatched plugins for ready ack after 10s
    if !dispatched_snapshot.is_empty() {
        crate::state::rt_handle().spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            for (url, load_id) in &dispatched_snapshot {
                let still_loading = with_state(|state| {
                    state.plugin_manager.is_loaded(url)
                        && !state.plugin_manager.is_ready(url)
                        && state.plugin_manager.current_load_id(url) == Some(*load_id)
                })
                .unwrap_or(false);
                if still_loading {
                    crate::vprintln!(
                        "[PLUGIN] Startup timeout: '{}' (gen={}) never ready — marking failed",
                        url,
                        load_id
                    );
                    let cleanup_js = crate::plugins::PluginManager::generate_unload_js(url);
                    eval_js(&cleanup_js);
                    with_state(|state| state.plugin_manager.mark_unloaded(url));
                    let url_disable = url.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        crate::state::db().call_plugins(move |pc| {
                            let _ = crate::plugins::store::disable(pc, &url_disable);
                        });
                    })
                    .await;
                    crate::app_state::emit_ipc_event_with_args("jsrt.plugin_failed", &[url]);
                }
            }
        });
    }
}

pub(crate) fn handle_jsrt_fire_and_forget(msg: &IpcMessage) {
    match msg.channel.as_str() {
        "jsrt.set_token" => {
            let token = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            with_state(|state| {
                state.captured_token = token.to_string();
            });
            super::scrub_pkce_verifier();
            crate::vprintln!("[PLUGIN] Token captured ({} chars)", token.len());
        }
        "jsrt.session_clear" => {
            handle_session_clear();
        }
        "jsrt.session_hard_reset" => {
            // Destructive: wipes cookies, storage, token. Debug-only guard.
            if crate::logging::log_level() >= 1 {
                handle_session_hard_reset();
            }
        }
        "jsrt.plugin_ready" => {
            let url = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            let load_id: u64 = msg
                .args
                .get(1)
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let nonce: u64 = msg
                .args
                .get(2)
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if !url.is_empty() {
                let accepted =
                    with_state(|state| state.plugin_manager.mark_ready(url, load_id, nonce))
                        .unwrap_or(false);
                if accepted {
                    crate::vprintln!("[PLUGIN] Ready: {} (gen={})", url, load_id);
                    crate::app_state::emit_ipc_event_with_args(
                        "jsrt.plugin_ready_confirmed",
                        &[url],
                    );
                } else {
                    crate::vprintln!(
                        "[PLUGIN] Stale ready ack ignored: {} (gen={})",
                        url,
                        load_id
                    );
                }
            }
        }
        "jsrt.enable_plugin" => {
            let url = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            if url.is_empty() {
                return;
            }
            let url_owned = url.to_owned();
            let (code, deps_ok) = crate::state::db().call_plugins(move |pc| {
                let code = crate::plugins::store::get_code(pc, &url_owned);
                let manifest: Option<String> = pc
                    .query_row(
                        "SELECT manifest FROM plugins WHERE url = ?1",
                        rusqlite::params![url_owned],
                        |row| row.get(0),
                    )
                    .ok();
                let deps_satisfied = manifest
                    .as_deref()
                    .map(|m| crate::plugins::store::check_dependencies_satisfied(pc, m).is_ok())
                    .unwrap_or(true);
                (code, deps_satisfied)
            });
            if !deps_ok {
                crate::vprintln!(
                    "[PLUGIN] Cannot enable '{}': deps not satisfied (safety net)",
                    url
                );
                return;
            }
            if let Some(code) = code {
                let (load_id, nonce) =
                    with_state(|state| state.plugin_manager.mark_loading(url)).unwrap_or((0, 0));
                match crate::plugins::PluginManager::transpile_and_wrap(url, &code, load_id, nonce)
                {
                    Ok(js) => {
                        let dispatched = eval_js(&js);
                        if dispatched {
                            let url_flag = url.to_owned();
                            let flag_ok = crate::state::db().call_plugins(move |pc| {
                                crate::plugins::store::mark_ever_dispatched(pc, &url_flag)
                            });
                            if flag_ok.is_err() {
                                // Flag failed — revert
                                crate::vprintln!(
                                    "[PLUGIN] Failed to persist ever_dispatched for '{}' — reverting",
                                    url
                                );
                                let cleanup =
                                    crate::plugins::PluginManager::generate_unload_js(url);
                                eval_js(&cleanup);
                                with_state(|state| state.plugin_manager.mark_unloaded(url));
                                let url_dis = url.to_owned();
                                crate::state::db().call_plugins(move |pc| {
                                    let _ = crate::plugins::store::disable(pc, &url_dis);
                                });
                            }
                        } else {
                            // No frame — revert
                            with_state(|state| state.plugin_manager.mark_unloaded(url));
                            let url_dis = url.to_owned();
                            crate::state::db().call_plugins(move |pc| {
                                let _ = crate::plugins::store::disable(pc, &url_dis);
                            });
                        }
                    }
                    Err(e) => {
                        crate::vprintln!("[PLUGIN] Failed to prepare '{}': {}", url, e);
                        with_state(|state| state.plugin_manager.mark_unloaded(url));
                        let url_dis = url.to_owned();
                        crate::state::db().call_plugins(move |pc| {
                            let _ = crate::plugins::store::disable(pc, &url_dis);
                        });
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
            let is_loaded =
                with_state(|state| state.plugin_manager.is_loaded(url)).unwrap_or(false);
            if is_loaded {
                // Dispatch cleanup first, bookkeeping after
                let cleanup_js = crate::plugins::PluginManager::generate_unload_js(url);
                crate::vprintln!(
                    "[PLUGIN] Unloading '{}' ({} bytes cleanup JS)",
                    url,
                    cleanup_js.len()
                );
                eval_js(&cleanup_js);
                with_state(|state| state.plugin_manager.mark_unloaded(url));
            } else {
                crate::vprintln!("[PLUGIN] No cleanup needed for '{}' (not loaded)", url);
            }
        }
        "jsrt.load_plugins" => {
            // When called via sendIpc (fire-and-forget, no callback), run inline.
            // When called via invokeIpc (with callback), routed to handle_plugin_ipc instead.
            do_load_plugins_inline();
        }
        _ => {
            crate::vprintln!("[JSRT] Unknown fire-and-forget channel: {}", msg.channel);
        }
    }
}
