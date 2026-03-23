use crate::app_state::{IpcCallback, IpcMessage, eval_js, with_state};

fn send_bun_command(
    pending: &crate::native_runtime::PendingMap,
    stdin_tx: &tokio::sync::mpsc::UnboundedSender<String>,
    id: &str,
    cmd: serde_json::Value,
) -> Result<tokio::sync::oneshot::Receiver<Result<serde_json::Value, String>>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    match pending.lock() {
        Ok(mut map) => {
            map.insert(id.to_string(), tx);
        }
        Err(e) => return Err(format!("pending lock poisoned: {e}")),
    }
    let line = serde_json::to_string(&cmd).unwrap_or_default();
    if stdin_tx.send(line).is_err() {
        match pending.lock() {
            Ok(mut map) => {
                map.remove(id);
            }
            Err(e) => {
                crate::vprintln!("[NATIVE] pending lock poisoned during cleanup: {e}");
            }
        }
        return Err("Bun stdin channel closed".to_string());
    }
    Ok(rx)
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

pub(crate) fn handle_plugin_ipc(msg: IpcMessage, callback: IpcCallback) {
    // plugin.fetch — async HTTP fetch for plugins (token injection, audit)
    if msg.channel == "plugin.fetch" {
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
        let id = msg.id.unwrap_or_default();
        let opts: crate::plugins::fetch::FetchOpts = serde_json::from_str(&opts_json)
            .unwrap_or_else(|_| serde_json::from_str("{}").unwrap());
        let token = with_state(|state| state.captured_token.clone()).unwrap_or_default();
        with_state(|state| {
            state.pending_ipc_callbacks.insert(id.clone(), callback);
            let rt = state.rt_handle.clone();
            rt.spawn(async move {
                let result =
                    crate::plugins::fetch::plugin_fetch(&plugin_id, &url, &opts, &token).await;
                let Some(cb) = take_ipc_callback(&id) else {
                    return;
                };
                match result {
                    Ok(json) => ipc_callback_ok(&cb, &json),
                    Err(e) => ipc_callback_err(&cb, &e),
                }
            });
        });
        return;
    }

    // player.parse_dash — synchronous DASH MPD XML parsing
    if msg.channel == "player.parse_dash" {
        let xml = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
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

    // __Luna.registerNative — register a native module in the Bun child process
    if msg.channel == "__Luna.registerNative" {
        let name = msg
            .args
            .first()
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let code = msg
            .args
            .get(1)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() || code.is_empty() {
            ipc_callback_err(&callback, "registerNative: missing name or code");
            return;
        }

        crate::vprintln!(
            "[NATIVE] Registering module '{}' ({} bytes)",
            name,
            code.len()
        );

        let spawn_data = with_state(|state| {
            if state.native_runtime.is_none() {
                match crate::native_runtime::NativeRuntime::spawn(&state.rt_handle) {
                    Ok(rt) => {
                        state.native_runtime = Some(rt);
                        crate::vprintln!("[NATIVE] Bun process started");
                    }
                    Err(e) => {
                        crate::vprintln!("[NATIVE] Failed to start Bun: {e}");
                        return Err(format!("Failed to start native runtime: {e}"));
                    }
                }
            }

            let pending = state.native_runtime.as_ref().unwrap().pending_clone();
            let stdin_tx = state.native_runtime.as_ref().unwrap().stdin_tx_clone();
            let bun_id = state.native_runtime.as_ref().unwrap().next_id_str();
            let rt = state.rt_handle.clone();
            Ok((pending, stdin_tx, bun_id, rt))
        });

        let Some(Ok((pending, stdin_tx, bun_id, rt))) = spawn_data else {
            if let Some(Err(e)) = spawn_data {
                ipc_callback_err(&callback, &e);
            }
            return;
        };

        rt.spawn(async move {
            let cmd = serde_json::json!({
                "id": bun_id,
                "type": "register",
                "name": name,
                "code": code,
            });
            let rx = match send_bun_command(&pending, &stdin_tx, &bun_id, cmd) {
                Ok(rx) => rx,
                Err(e) => {
                    ipc_callback_err(&callback, &e);
                    return;
                }
            };

            match rx.await {
                Ok(Ok(response)) => {
                    let exports = response
                        .get("exports")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let channel = format!("__LunaNative.{name}");
                    crate::vprintln!(
                        "[NATIVE] Registered '{}': {} exports ({})",
                        name,
                        exports.len(),
                        exports.join(", ")
                    );
                    ipc_callback_ok(&callback, &format!("\"{channel}\""));
                }
                Ok(Err(e)) => {
                    crate::vprintln!("[NATIVE] Register failed for '{}': {}", name, e);
                    ipc_callback_err(&callback, &e);
                }
                Err(_) => {
                    ipc_callback_err(&callback, "Bun response channel dropped");
                }
            }
        });
        return;
    }

    // __LunaNative.* — call a function on a registered native module
    if msg.channel.starts_with("__LunaNative.") {
        let module_name = msg
            .channel
            .strip_prefix("__LunaNative.")
            .unwrap_or("")
            .to_string();
        let export_name = msg
            .args
            .first()
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let call_args: Vec<serde_json::Value> = msg.args.iter().skip(1).cloned().collect();

        let call_data = with_state(|state| {
            let Some(ref runtime) = state.native_runtime else {
                return Err("Native runtime not initialized".to_string());
            };

            let pending = runtime.pending_clone();
            let stdin_tx = runtime.stdin_tx_clone();
            let bun_id = runtime.next_id_str();
            let rt = state.rt_handle.clone();
            Ok((pending, stdin_tx, bun_id, rt))
        });

        let Some(Ok((pending, stdin_tx, bun_id, rt))) = call_data else {
            if let Some(Err(e)) = call_data {
                ipc_callback_err(&callback, &e);
            }
            return;
        };

        rt.spawn(async move {
            let cmd = serde_json::json!({
                "id": bun_id,
                "type": "call",
                "name": module_name,
                "fn": export_name,
                "args": call_args,
            });
            let rx = match send_bun_command(&pending, &stdin_tx, &bun_id, cmd) {
                Ok(rx) => rx,
                Err(e) => {
                    ipc_callback_err(&callback, &e);
                    return;
                }
            };

            match rx.await {
                Ok(Ok(response)) => {
                    let val = response
                        .get("result")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    ipc_callback_ok(&callback, &val.to_string());
                }
                Ok(Err(e)) => ipc_callback_err(&callback, &e),
                Err(_) => ipc_callback_err(&callback, "Bun response channel dropped"),
            }
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
                let Some(cb) = take_ipc_callback(&id) else {
                    return;
                };
                match result {
                    Ok(manifest) => ipc_callback_ok(&cb, &manifest),
                    Err(e) => ipc_callback_err(&cb, &format!("{e:#}")),
                }
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

    let db = crate::state::db();
    let channel = msg.channel.clone();
    let args = msg.args.clone();

    let result: Result<String, String> = db.call(move |pc, _| match channel.as_str() {
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
            crate::plugins::store::uninstall(pc, url)
                .map(|()| "true".to_string())
                .map_err(|e| e.to_string())
        }
        "plugin.uninstall_all" => crate::plugins::store::uninstall_all(pc)
            .map(|()| "true".to_string())
            .map_err(|e| e.to_string()),
        "plugin.enable" => {
            let url = args.first().and_then(|v| v.as_str()).unwrap_or("");
            crate::plugins::store::enable(pc, url)
                .map(|()| "true".to_string())
                .map_err(|e| e.to_string())
        }
        "plugin.disable" => {
            let url = args.first().and_then(|v| v.as_str()).unwrap_or("");
            crate::plugins::store::disable(pc, url)
                .map(|()| "true".to_string())
                .map_err(|e| e.to_string())
        }
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
        _ => Err(format!("Unknown plugin channel: {}", channel)),
    });

    match result {
        Ok(json) => ipc_callback_ok(&callback, &json),
        Err(e) => ipc_callback_err(&callback, &e),
    }
}

async fn handle_proxy_head(id: String, url: String) {
    let client = &*crate::state::HTTP_CLIENT;
    let result = client.head(&url).send().await;

    let Some(callback) = take_ipc_callback(&id) else {
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
}

async fn handle_proxy_fetch(id: String, url: String, headers_json: String) {
    let client = &*crate::state::HTTP_CLIENT;
    let mut req = client.get(&url);

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

    let Some(callback) = take_ipc_callback(&id) else {
        return;
    };
    match result {
        Ok(resp) => {
            let status = resp.status().as_u16();
            tokio::spawn(async move {
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
}

async fn handle_plugin_install(id: String, url: String) {
    let client = &*crate::state::HTTP_CLIENT;
    let result = fetch_plugin_data(client, &url).await;

    let Some(callback) = take_ipc_callback(&id) else {
        return;
    };
    match result {
        Ok((name, manifest, code, hash)) => {
            let install_result = tokio::task::spawn_blocking({
                let url = url.clone();
                move || {
                    crate::state::db().call(move |pc, _| {
                        crate::plugins::store::install(pc, &url, &name, &manifest, &code, &hash)
                            .map(|()| crate::plugins::PluginInfo {
                                url,
                                name,
                                manifest,
                                hash: Some(hash),
                                enabled: true,
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
    use std::hash::{Hash, Hasher};

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
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
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
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        code.hash(&mut hasher);
        let hash = format!("{:x}", hasher.finish());

        Ok((name, manifest, code, hash))
    }
}
