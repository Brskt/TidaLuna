use super::{ipc_callback_err, ipc_callback_ok, take_ipc_callback};
use crate::app_state::{IpcCallback, IpcMessage, with_state};

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
    let id = msg.id.clone().unwrap_or_default();
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
        let rt = state.rt_handle.clone();
        rt.spawn(async move {
            do_plugin_install(id, url).await;
        });
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

async fn do_plugin_install(id: String, url: String) {
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
                    crate::state::db().call_plugins(move |pc| {
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
