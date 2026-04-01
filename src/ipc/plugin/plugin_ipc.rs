use super::{ipc_callback_err, ipc_callback_ok, take_ipc_callback};
use crate::app_state::{IpcCallback, IpcMessage, eval_js, with_state};

pub(super) fn handle_plugin_fetch(msg: &IpcMessage, callback: IpcCallback) {
    let plugin_id = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let url = msg
        .args
        .get(1)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let opts_json = msg
        .args
        .get(2)
        .and_then(|v| v.as_str())
        .unwrap_or("{}")
        .to_string();

    // Block non-Tidal requests that carry a real token in URL or opts (body/headers).
    if !crate::plugins::fetch::is_tidal_api(&url) {
        let payload = if opts_json == "{}" {
            None
        } else {
            Some(opts_json.as_str())
        };
        if super::proxy::leaks_real_token(&url, payload) {
            crate::vprintln!(
                "[PLUGIN:FETCH] BLOCKED token exfiltration from '{}' to {}",
                plugin_id,
                &url[..url.len().min(80)]
            );
            ipc_callback_err(&callback, "plugin.fetch: request blocked");
            return;
        }
    }

    let id = msg.id.clone().unwrap_or_default();
    let opts: crate::plugins::fetch::FetchOpts =
        serde_json::from_str(&opts_json).unwrap_or_else(|_| serde_json::from_str("{}").unwrap());
    let token = with_state(|state| state.captured_token.clone()).unwrap_or_default();
    with_state(|state| {
        state.pending_ipc_callbacks.insert(id.clone(), callback);
    });
    crate::state::rt_handle().spawn(async move {
        let result = crate::plugins::fetch::plugin_fetch(&plugin_id, &url, &opts, &token).await;
        let Some(cb) = take_ipc_callback(&id) else {
            return;
        };
        match result {
            Ok(json) => ipc_callback_ok(&cb, &json),
            Err(e) => ipc_callback_err(&cb, &e),
        }
    });
}

/// Authenticated fetch restricted to TIDAL API hosts.
/// Used by core bundle (@luna/lib) — the token never leaves Rust.
pub(super) fn handle_tidal_fetch(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let opts_json = msg
        .args
        .get(1)
        .and_then(|v| v.as_str())
        .unwrap_or("{}")
        .to_string();
    let id = msg.id.clone().unwrap_or_default();

    // Reject non-TIDAL URLs — this channel only serves authenticated TIDAL API requests
    if !crate::plugins::fetch::is_tidal_api(&url) {
        ipc_callback_err(
            &callback,
            &format!("tidal.fetch rejected: not a TIDAL API URL ({url})"),
        );
        return;
    }

    let opts: crate::plugins::fetch::FetchOpts =
        serde_json::from_str(&opts_json).unwrap_or_else(|_| serde_json::from_str("{}").unwrap());
    let token = with_state(|state| state.captured_token.clone()).unwrap_or_default();

    // Reject if the OAuth token hasn't been captured yet — avoids sending unauthenticated
    // requests that would 403 and get memoized as failures on the frontend.
    if token.is_empty() {
        ipc_callback_err(&callback, "tidal.fetch: auth token not yet captured");
        return;
    }

    with_state(|state| {
        state.pending_ipc_callbacks.insert(id.clone(), callback);
    });
    crate::state::rt_handle().spawn(async move {
        let result = crate::plugins::fetch::plugin_fetch("@luna/lib", &url, &opts, &token).await;
        let Some(cb) = take_ipc_callback(&id) else {
            return;
        };
        match result {
            Ok(json) => ipc_callback_ok(&cb, &json),
            Err(e) => ipc_callback_err(&cb, &e),
        }
    });
}

pub(super) fn handle_plugin_fetch_package(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let id = msg.id.clone().unwrap_or_default();
    with_state(|state| {
        state.pending_ipc_callbacks.insert(id.clone(), callback);
    });
    crate::state::rt_handle().spawn(async move {
        let client = &*crate::state::HTTP_CLIENT;
        let result = fetch_plugin_package(client, &url).await;
        let Some(cb) = take_ipc_callback(&id) else {
            return;
        };
        match result {
            Ok(manifest) => ipc_callback_ok(&cb, &manifest),
            Err(e) => ipc_callback_err(&cb, &format!("{e:#}")),
        }
    });
}

pub(super) fn handle_plugin_install(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let id = msg.id.clone().unwrap_or_default();
    with_state(|state| {
        state.pending_ipc_callbacks.insert(id.clone(), callback);
    });
    crate::state::rt_handle().spawn(async move {
        do_plugin_install(id, url).await;
    });
}

/// Atomic enable: check deps → DB enable → transpile → eval_js → mark_loaded.
/// DB-first: if transpile/inject fails, reverts enabled to 0.
pub(super) fn handle_plugin_enable(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let id = msg.id.clone().unwrap_or_default();
    with_state(|state| {
        state.pending_ipc_callbacks.insert(id.clone(), callback);
    });
    crate::state::rt_handle().spawn(async move {
        do_plugin_enable(id, url).await;
    });
}

async fn do_plugin_enable(id: String, url: String) {
    let db = crate::state::db();

    // 1. Check deps + set enabled=1 + get code (all DB ops)
    let db_result: Result<(String, String), String> = {
        let url = url.clone();
        match tokio::task::spawn_blocking(move || {
            db.call_plugins(move |pc| {
                // Get manifest and code
                let (manifest, code): (String, String) = pc
                    .query_row(
                        "SELECT manifest, code FROM plugins WHERE url = ?1 AND installed = 1",
                        rusqlite::params![url],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(|e| format!("Plugin not found or no code: {e}"))?;

                // Check dependencies are satisfied
                if let Err(missing) =
                    crate::plugins::store::check_dependencies_satisfied(pc, &manifest)
                {
                    return Err(format!(
                        "Cannot enable: deps not satisfied: {}",
                        missing.join(", ")
                    ));
                }

                // DB: set enabled=1
                crate::plugins::store::enable(pc, &url).map_err(|e| format!("DB error: {e}"))?;

                Ok((manifest, code))
            })
        })
        .await
        {
            Ok(r) => r,
            Err(e) => Err(format!("spawn_blocking failed: {e}")),
        }
    };

    let Some(callback) = take_ipc_callback(&id) else {
        return;
    };

    let (_manifest, code) = match db_result {
        Ok(v) => v,
        Err(e) => {
            ipc_callback_err(&callback, &e);
            return;
        }
    };

    // 2. Mark loading (generates load_id for wrapper correlation)
    let (load_id, nonce) =
        with_state(|state| state.plugin_manager.mark_loading(&url)).unwrap_or((0, 0));

    // 3. Transpile + wrap (load_id + nonce injected into wrapper for ack)
    let js = match crate::plugins::PluginManager::transpile_and_wrap(&url, &code, load_id, nonce) {
        Ok(js) => js,
        Err(e) => {
            // Revert in-memory + DB
            with_state(|state| state.plugin_manager.mark_unloaded(&url));
            let revert_url = url.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let _ = crate::state::db()
                    .call_plugins(move |pc| crate::plugins::store::disable(pc, &revert_url));
            })
            .await;
            ipc_callback_err(&callback, &format!("Failed to prepare: {e}"));
            return;
        }
    };

    // 4. Dispatch to renderer
    let dispatched = eval_js(&js);
    if !dispatched {
        // No renderer frame — revert everything
        with_state(|state| state.plugin_manager.mark_unloaded(&url));
        let revert_url = url.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let _ = crate::state::db()
                .call_plugins(move |pc| crate::plugins::store::disable(pc, &revert_url));
        })
        .await;
        ipc_callback_err(&callback, "No renderer frame available");
        return;
    }

    // 5. Persistent flag: code was dispatched — critical for cleanup guard correctness.
    //    If this fails, revert the activation: better a failed enable than an inconsistent flag.
    {
        let url_flag = url.clone();
        let flag_result = tokio::task::spawn_blocking(move || {
            crate::state::db()
                .call_plugins(move |pc| crate::plugins::store::mark_ever_dispatched(pc, &url_flag))
        })
        .await;
        let flag_ok = matches!(flag_result, Ok(Ok(())));
        if !flag_ok {
            crate::vprintln!(
                "[PLUGIN] Failed to persist ever_dispatched for '{}' — reverting activation",
                url
            );
            let cleanup_js = crate::plugins::PluginManager::generate_unload_js(&url);
            eval_js(&cleanup_js);
            with_state(|state| state.plugin_manager.mark_unloaded(&url));
            let revert_url = url.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let _ = crate::state::db()
                    .call_plugins(move |pc| crate::plugins::store::disable(pc, &revert_url));
            })
            .await;
            ipc_callback_err(&callback, "Failed to persist dispatch flag");
            return;
        }
    }

    // 6. Timeout: if no ready ack within 10s, mark failed + notify frontend
    {
        let timeout_url = url.clone();
        crate::state::rt_handle().spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let still_loading = with_state(|state| {
                state.plugin_manager.is_loaded(&timeout_url)
                    && !state.plugin_manager.is_ready(&timeout_url)
                    && state.plugin_manager.current_load_id(&timeout_url) == Some(load_id)
            })
            .unwrap_or(false);
            if still_loading {
                crate::vprintln!(
                    "[PLUGIN] Timeout: '{}' never sent ready ack (gen={}) — marking failed",
                    timeout_url,
                    load_id
                );
                let cleanup_js = crate::plugins::PluginManager::generate_unload_js(&timeout_url);
                eval_js(&cleanup_js);
                with_state(|state| state.plugin_manager.mark_unloaded(&timeout_url));
                let url_disable = timeout_url.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    crate::state::db().call_plugins(move |pc| {
                        let _ = crate::plugins::store::disable(pc, &url_disable);
                    });
                })
                .await;
                crate::app_state::emit_ipc_event_with_args("jsrt.plugin_failed", &[&timeout_url]);
            }
        });
    }

    ipc_callback_ok(&callback, "true");
}

/// Atomic disable: guard dependants → DB disable → eval_js cleanup → mark_unloaded.
pub(super) fn handle_plugin_disable(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let id = msg.id.clone().unwrap_or_default();
    with_state(|state| {
        state.pending_ipc_callbacks.insert(id.clone(), callback);
    });
    crate::state::rt_handle().spawn(async move {
        do_plugin_disable(id, url).await;
    });
}

async fn do_plugin_disable(id: String, url: String) {
    let db = crate::state::db();

    // 1. Guard: check enabled dependants + DB disable
    let guard_result: Result<(), String> = {
        let url = url.clone();
        match tokio::task::spawn_blocking(move || {
            db.call_plugins(move |pc| {
                let name = crate::plugins::store::get_name_by_url(pc, &url)
                    .ok_or_else(|| format!("Plugin not found: {url}"))?;

                match crate::plugins::store::find_enabled_dependants(pc, &name) {
                    Err(msg) => return Err(msg),
                    Ok(deps) if !deps.is_empty() => {
                        return Err(format!(
                            "Cannot disable '{name}': active dependants: {}",
                            deps.join(", ")
                        ));
                    }
                    _ => {}
                }

                // DB: set enabled=0
                crate::plugins::store::disable(pc, &url).map_err(|e| format!("DB error: {e}"))?;
                Ok(())
            })
        })
        .await
        {
            Ok(r) => r,
            Err(e) => Err(format!("spawn_blocking failed: {e}")),
        }
    };

    let Some(callback) = take_ipc_callback(&id) else {
        return;
    };

    if let Err(e) = guard_result {
        ipc_callback_err(&callback, &e);
        return;
    }

    // 2. Unload from renderer (best-effort)
    let cleanup_js = crate::plugins::PluginManager::generate_unload_js(&url);
    eval_js(&cleanup_js);

    // 3. Bookkeeping AFTER dispatch
    with_state(|state| state.plugin_manager.mark_unloaded(&url));

    ipc_callback_ok(&callback, "true");
}

/// jsrt.load_plugins as request-response: runs multi-pass + reconciliation, then responds.
/// Refuses to load plugins when no session is active (login page scenario).
pub(super) fn handle_jsrt_load_plugins(callback: IpcCallback) {
    let has_session =
        with_state(|state| !state.captured_token.is_empty() || state.token_state.is_some())
            .unwrap_or(false);

    if !has_session {
        crate::vprintln!("[PLUGIN] Skipping plugin load — no active session");
        ipc_callback_ok(&callback, "true");
        return;
    }

    super::jsrt::do_load_plugins_inline();
    ipc_callback_ok(&callback, "true");
}

/// Check for code changes without modifying the DB. Returns `{ hash: "..." }` or error.
/// Used by live reload polling.
pub(super) fn handle_plugin_check_hash(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let id = msg.id.clone().unwrap_or_default();
    with_state(|state| {
        state.pending_ipc_callbacks.insert(id.clone(), callback);
    });
    crate::state::rt_handle().spawn(async move {
        let client = &*crate::state::HTTP_CLIENT;
        let base = sanitize_plugin_url(&url);
        let code_url = format!("{base}.mjs");

        let Some(cb) = take_ipc_callback(&id) else {
            return;
        };

        match client.get(&code_url).send().await {
            Ok(resp) => match resp.error_for_status() {
                Ok(resp) => match resp.text().await {
                    Ok(code) => {
                        use std::hash::{Hash, Hasher as _};
                        let mut hasher = fnv::FnvHasher::default();
                        code.hash(&mut hasher);
                        let hash = format!("{:x}", hasher.finish());
                        let json = format!(r#"{{"hash":"{hash}"}}"#);
                        ipc_callback_ok(&cb, &json);
                    }
                    Err(e) => ipc_callback_err(&cb, &format!("{e}")),
                },
                Err(e) => ipc_callback_err(&cb, &format!("{e}")),
            },
            Err(e) => ipc_callback_err(&cb, &format!("{e}")),
        }
    });
}

pub(super) fn handle_plugin_db(msg: IpcMessage, callback: IpcCallback) {
    let db = crate::state::db();
    let channel = msg.channel.clone();
    let args = msg.args.clone();

    let result: Result<String, String> = db.call_plugins(move |pc| match channel.as_str() {
        "plugin.list" => {
            let plugins = crate::plugins::store::list(pc);
            Ok(serde_json::to_string(&plugins).unwrap_or_else(|_| "[]".to_string()))
        }
        "plugin.get_code" => {
            let url = args.first().and_then(|v| v.as_str()).unwrap_or("");
            match crate::plugins::store::get_code(pc, url) {
                Some(code) => {
                    Ok(serde_json::to_string(&code).unwrap_or_else(|_| "null".to_string()))
                }
                None => Ok("null".to_string()),
            }
        }
        "plugin.uninstall" => {
            let url = args.first().and_then(|v| v.as_str()).unwrap_or("");
            // Guard: fail-closed check for dependants
            let name = crate::plugins::store::get_name_by_url(pc, url).unwrap_or_default();
            if !name.is_empty() {
                match crate::plugins::store::find_dependants(pc, &name) {
                    Err(msg) => return Err(msg),
                    Ok(deps) if !deps.is_empty() => {
                        return Err(format!(
                            "Cannot uninstall '{name}': required by {}",
                            deps.join(", ")
                        ));
                    }
                    _ => {}
                }
            }
            crate::plugins::store::uninstall(pc, url)
                .map(|()| "true".to_string())
                .map_err(|e| e.to_string())
        }
        "plugin.uninstall_all" => crate::plugins::store::uninstall_all(pc)
            .map(|()| "true".to_string())
            .map_err(|e| e.to_string()),
        "plugin.storage.get" => {
            let ns = args.first().and_then(|v| v.as_str()).unwrap_or("");
            let key = args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            match crate::plugins::store::storage_get(pc, ns, key) {
                Some(val) => Ok(serde_json::to_string(&val).unwrap_or_else(|_| "null".to_string())),
                None => Ok("null".to_string()),
            }
        }
        "plugin.storage.set" => {
            let ns = args.first().and_then(|v| v.as_str()).unwrap_or("");
            let key = args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            let value = args.get(2).and_then(|v| v.as_str()).unwrap_or("");
            crate::plugins::store::storage_set(pc, ns, key, value)
                .map(|()| "true".to_string())
                .map_err(|e| e.to_string())
        }
        "plugin.storage.del" => {
            let ns = args.first().and_then(|v| v.as_str()).unwrap_or("");
            let key = args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            crate::plugins::store::storage_del(pc, ns, key)
                .map(|()| "true".to_string())
                .map_err(|e| e.to_string())
        }
        "plugin.storage.keys" => {
            let ns = args.first().and_then(|v| v.as_str()).unwrap_or("");
            let keys = crate::plugins::store::storage_keys(pc, ns);
            Ok(serde_json::to_string(&keys).unwrap_or_else(|_| "[]".to_string()))
        }
        "plugin.cleanup_failed_install" => {
            let url = args.first().and_then(|v| v.as_str()).unwrap_or("");
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
    });

    // On successful uninstall, clear native trust decisions so reinstalling re-prompts.
    let channel_ref = msg.channel.as_str();
    if result.is_ok()
        && (channel_ref == "plugin.uninstall" || channel_ref == "plugin.uninstall_all")
    {
        let name = if channel_ref == "plugin.uninstall" {
            msg.args
                .first()
                .and_then(|v| v.as_str())
                .and_then(|url| {
                    // Re-derive plugin name from URL (same logic as the uninstall branch)
                    url.rsplit_once('/').map(|(_, n)| n.to_string())
                })
                .unwrap_or_default()
        } else {
            "%".to_string() // uninstall_all → clear all trust
        };
        if !name.is_empty() {
            let db_name = name.clone();
            crate::state::db().call_settings(move |conn| {
                let _ = crate::native_runtime::trust::clear_trust_by_plugin(conn, &db_name);
            });
            // Clear in-memory watch channels so reinstall re-prompts
            super::native::clear_pending_trust(&name);
        }
    }

    match result {
        Ok(json) => ipc_callback_ok(&callback, &json),
        Err(e) => ipc_callback_err(&callback, &e),
    }
}

// --- Fetch helpers ---

fn sanitize_plugin_url(url: &str) -> &str {
    url.trim_end_matches(".mjs.map")
        .trim_end_matches(".mjs")
        .trim_end_matches(".json")
}

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
    let _: serde_json::Value = serde_json::from_str(&manifest_str)?;
    Ok(manifest_str)
}

async fn fetch_plugin_data(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<(String, String, String, String)> {
    use std::hash::{Hash, Hasher as _};

    let base = sanitize_plugin_url(url);

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
        let mut hasher = fnv::FnvHasher::default();
        code.hash(&mut hasher);
        let hash = format!("{:x}", hasher.finish());

        Ok((name, manifest_str, code, hash))
    } else {
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
        let mut hasher = fnv::FnvHasher::default();
        code.hash(&mut hasher);
        let hash = format!("{:x}", hasher.finish());

        Ok((name, manifest, code, hash))
    }
}

async fn do_plugin_install(id: String, url: String) {
    let client = &*crate::state::HTTP_CLIENT;
    let result = fetch_plugin_data(client, &url).await;

    let Some(callback) = take_ipc_callback(&id) else {
        return;
    };
    match result {
        Ok((name, manifest, code, hash)) => {
            // Validate luna.type if present
            match crate::plugins::store::parse_luna_meta(&manifest) {
                Err(msg) => {
                    ipc_callback_err(&callback, &msg);
                    return;
                }
                Ok(Some(ref meta)) => {
                    if let Some(ref t) = meta.plugin_type
                        && t != "plugin"
                        && t != "library"
                    {
                        ipc_callback_err(
                            &callback,
                            &format!("Invalid luna.type '{t}' in manifest"),
                        );
                        return;
                    }
                }
                Ok(None) => {} // no luna field — fine
            }

            let install_result = tokio::task::spawn_blocking({
                let url = url.clone();
                move || {
                    crate::state::db().call_plugins(move |pc| {
                        crate::plugins::store::install(pc, &url, &name, &manifest, &code, &hash)
                            .map(|()| crate::plugins::PluginInfo {
                                url,
                                name,
                                manifest,
                                hash: Some(hash),
                                enabled: false,
                                installed: true,
                            })
                    })
                }
            })
            .await;
            match install_result {
                Ok(Ok(info)) => {
                    let json = serde_json::to_string(&info).unwrap_or_else(|_| "null".to_string());
                    ipc_callback_ok(&callback, &json);
                }
                Ok(Err(e)) => ipc_callback_err(&callback, &e.to_string()),
                Err(e) => ipc_callback_err(&callback, &format!("spawn_blocking failed: {e}")),
            }
        }
        Err(e) => ipc_callback_err(&callback, &format!("{e:#}")),
    }
}
