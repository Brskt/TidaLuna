use crate::app_state::{IpcCallback, IpcMessage, eval_js, with_state};

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
            let prepared = with_state(|state| {
                let code = state.plugin_store.get_code(url)?;
                match state.plugin_manager.prepare_plugin(url, &code) {
                    Ok(js) => Some(js),
                    Err(e) => {
                        crate::vprintln!("[PLUGIN] Failed to prepare '{}': {}", url, e);
                        None
                    }
                }
            })
            .flatten();
            if let Some(js) = prepared {
                eval_js(&js);
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
            let prepared = with_state(|state| {
                state
                    .plugin_manager
                    .prepare_all_enabled(&state.plugin_store)
            })
            .unwrap_or_default();
            crate::vprintln!("[PLUGIN] Loading {} plugins into CEF", prepared.len());
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
                with_state(|state| {
                    let Some(cb) = state.pending_ipc_callbacks.remove(&id) else {
                        return;
                    };
                    match result {
                        Ok(json) => ipc_callback_ok(&cb, &json),
                        Err(e) => ipc_callback_err(&cb, &e),
                    }
                });
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

        with_state(|state| {
            // Lazy-init Bun process
            if state.native_runtime.is_none() {
                match crate::native_runtime::NativeRuntime::spawn(&state.rt_handle) {
                    Ok(rt) => {
                        state.native_runtime = Some(rt);
                        crate::vprintln!("[NATIVE] Bun process started");
                    }
                    Err(e) => {
                        crate::vprintln!("[NATIVE] Failed to start Bun: {e}");
                        ipc_callback_err(
                            &callback,
                            &format!("Failed to start native runtime: {e}"),
                        );
                        return;
                    }
                }
            }

            let pending = state.native_runtime.as_ref().unwrap().pending_clone();
            let stdin_tx = state.native_runtime.as_ref().unwrap().stdin_tx_clone();
            let bun_id = state.native_runtime.as_ref().unwrap().next_id_str();
            let rt = state.rt_handle.clone();

            rt.spawn(async move {
                let cmd = serde_json::json!({
                    "id": bun_id,
                    "type": "register",
                    "name": name,
                    "code": code,
                });
                let (tx, rx) = tokio::sync::oneshot::channel();
                {
                    if let Ok(mut map) = pending.lock() {
                        map.insert(bun_id.clone(), tx);
                    }
                }
                let line = serde_json::to_string(&cmd).unwrap_or_default();
                let _ = stdin_tx.send(line);

                let result = rx.await;

                match result {
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

        with_state(|state| {
            let Some(ref runtime) = state.native_runtime else {
                ipc_callback_err(&callback, "Native runtime not initialized");
                return;
            };

            let pending = runtime.pending_clone();
            let stdin_tx = runtime.stdin_tx_clone();
            let bun_id = runtime.next_id_str();
            let rt = state.rt_handle.clone();

            rt.spawn(async move {
                let cmd = serde_json::json!({
                    "id": bun_id,
                    "type": "call",
                    "name": module_name,
                    "fn": export_name,
                    "args": call_args,
                });
                let (tx, rx) = tokio::sync::oneshot::channel();
                {
                    if let Ok(mut map) = pending.lock() {
                        map.insert(bun_id.clone(), tx);
                    }
                }
                let line = serde_json::to_string(&cmd).unwrap_or_default();
                let _ = stdin_tx.send(line);

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
                with_state(|state| {
                    let Some(cb) = state.pending_ipc_callbacks.remove(&id) else {
                        return;
                    };
                    match result {
                        Ok(manifest) => ipc_callback_ok(&cb, &manifest),
                        Err(e) => ipc_callback_err(&cb, &format!("{e:#}")),
                    }
                });
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

    with_state(|state| match msg.channel.as_str() {
        "plugin.list" => {
            let plugins = state.plugin_store.list();
            let json = serde_json::to_string(&plugins).unwrap_or_else(|_| "[]".to_string());
            ipc_callback_ok(&callback, &json);
        }
        "plugin.get_code" => {
            let url = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            match state.plugin_store.get_code(url) {
                Some(code) => {
                    let json = serde_json::to_string(&code).unwrap_or_else(|_| "null".to_string());
                    ipc_callback_ok(&callback, &json);
                }
                None => ipc_callback_ok(&callback, "null"),
            }
        }
        "plugin.uninstall" => {
            let url = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            match state.plugin_store.uninstall(url) {
                Ok(()) => ipc_callback_ok(&callback, "true"),
                Err(e) => ipc_callback_err(&callback, &e.to_string()),
            }
        }
        "plugin.uninstall_all" => match state.plugin_store.uninstall_all() {
            Ok(()) => ipc_callback_ok(&callback, "true"),
            Err(e) => ipc_callback_err(&callback, &e.to_string()),
        },
        "plugin.enable" => {
            let url = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            match state.plugin_store.enable(url) {
                Ok(()) => ipc_callback_ok(&callback, "true"),
                Err(e) => ipc_callback_err(&callback, &e.to_string()),
            }
        }
        "plugin.disable" => {
            let url = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            match state.plugin_store.disable(url) {
                Ok(()) => ipc_callback_ok(&callback, "true"),
                Err(e) => ipc_callback_err(&callback, &e.to_string()),
            }
        }
        "plugin.storage.get" => {
            let ns = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            let key = msg.args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            match state.plugin_store.storage_get(ns, key) {
                Some(val) => {
                    let json = serde_json::to_string(&val).unwrap_or_else(|_| "null".to_string());
                    ipc_callback_ok(&callback, &json);
                }
                None => ipc_callback_ok(&callback, "null"),
            }
        }
        "plugin.storage.set" => {
            let ns = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            let key = msg.args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            let value = msg.args.get(2).and_then(|v| v.as_str()).unwrap_or("");
            match state.plugin_store.storage_set(ns, key, value) {
                Ok(()) => ipc_callback_ok(&callback, "true"),
                Err(e) => ipc_callback_err(&callback, &e.to_string()),
            }
        }
        "plugin.storage.del" => {
            let ns = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            let key = msg.args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            match state.plugin_store.storage_del(ns, key) {
                Ok(()) => ipc_callback_ok(&callback, "true"),
                Err(e) => ipc_callback_err(&callback, &e.to_string()),
            }
        }
        "plugin.storage.keys" => {
            let ns = msg.args.first().and_then(|v| v.as_str()).unwrap_or("");
            let keys = state.plugin_store.storage_keys(ns);
            let json = serde_json::to_string(&keys).unwrap_or_else(|_| "[]".to_string());
            ipc_callback_ok(&callback, &json);
        }
        _ => {
            ipc_callback_err(
                &callback,
                &format!("Unknown plugin channel: {}", msg.channel),
            );
        }
    });
}

async fn handle_proxy_head(id: String, url: String) {
    let client = &*crate::state::HTTP_CLIENT;
    let result = client.head(&url).send().await;

    with_state(|state| {
        let Some(callback) = state.pending_ipc_callbacks.remove(&id) else {
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
    });
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

    with_state(|state| {
        let Some(callback) = state.pending_ipc_callbacks.remove(&id) else {
            return;
        };
        match result {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let rt = state.rt_handle.clone();
                rt.spawn(async move {
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
    });
}

async fn handle_plugin_install(id: String, url: String) {
    let client = &*crate::state::HTTP_CLIENT;
    let result = fetch_plugin_data(client, &url).await;

    with_state(|state| {
        let Some(callback) = state.pending_ipc_callbacks.remove(&id) else {
            return;
        };
        match result {
            Ok((name, manifest, code, hash)) => {
                match state
                    .plugin_store
                    .install(&url, &name, &manifest, &code, &hash)
                {
                    Ok(()) => {
                        let info = crate::plugins::PluginInfo {
                            url: url.clone(),
                            name,
                            manifest,
                            hash: Some(hash),
                            enabled: true,
                            installed: true,
                        };
                        let json =
                            serde_json::to_string(&info).unwrap_or_else(|_| "null".to_string());
                        ipc_callback_ok(&callback, &json);
                    }
                    Err(e) => ipc_callback_err(&callback, &e.to_string()),
                }
            }
            Err(e) => ipc_callback_err(&callback, &format!("{e:#}")),
        }
    });
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
