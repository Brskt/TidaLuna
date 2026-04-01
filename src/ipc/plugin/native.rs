use super::{ipc_callback_err, ipc_callback_ok};
use crate::app_state::{IpcCallback, IpcMessage};
use crate::native_runtime::NativeRuntime;
use crate::state::{NATIVE_RUNTIME, NATIVE_RUNTIME_INIT};

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use tokio::sync::watch;

/// Pending trust requests: keyed by "name::module".
/// Uses watch channels so multiple concurrent register calls for the same
/// plugin can share a single dialog prompt.
type TrustMap = HashMap<String, watch::Sender<Option<bool>>>;

static PENDING_TRUST: LazyLock<Mutex<TrustMap>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Clear in-memory watch channels for a plugin (called on uninstall).
/// Keys are "name::module", so we remove any key starting with the plugin prefix.
pub(super) fn clear_pending_trust(plugin_prefix: &str) {
    let mut guard = PENDING_TRUST.lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|key, _| !key.starts_with(plugin_prefix));
}

fn compute_code_hash(code: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(code.trim().as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Initialize the native runtime (Bun child process) if not already running.
fn ensure_native_runtime() -> Result<&'static NativeRuntime, String> {
    if let Some(rt) = NATIVE_RUNTIME.get() {
        return Ok(rt);
    }
    let _guard = NATIVE_RUNTIME_INIT
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(rt) = NATIVE_RUNTIME.get() {
        return Ok(rt);
    }
    let rt = NativeRuntime::spawn(crate::state::rt_handle())
        .map_err(|e| format!("Failed to start native runtime: {e}"))?;
    crate::vprintln!("[NATIVE] Bun process started");
    if NATIVE_RUNTIME.set(rt).is_err() {
        panic!("NATIVE_RUNTIME already initialized under init lock");
    }
    Ok(NATIVE_RUNTIME.get().unwrap())
}

pub(super) fn handle_register_native(msg: &IpcMessage, callback: IpcCallback) {
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

    // Validate that the plugin prefix maps to a currently loaded plugin.
    // Prevents a malicious plugin from calling registerNative with another
    // plugin's name to inherit its trust grants.
    // "DiscordRPC/discord.native.ts" → "DiscordRPC"
    // "@scope/pkg/foo.native.ts" → "@scope/pkg"
    let plugin_prefix = name.rsplit_once('/').map(|(p, _)| p).unwrap_or(&name);
    let is_active = crate::app_state::with_state(|state| {
        state
            .plugin_manager
            .url_for_name(plugin_prefix)
            .is_some_and(|url| state.plugin_manager.is_loaded(url))
    })
    .unwrap_or(false);
    if !is_active {
        crate::vprintln!(
            "[NATIVE] Rejected registerNative for '{}': plugin not active",
            name
        );
        ipc_callback_err(
            &callback,
            &format!("registerNative: plugin '{}' is not active", plugin_prefix),
        );
        return;
    }

    crate::vprintln!(
        "[NATIVE] Registering module '{}' ({} bytes)",
        name,
        code.len()
    );

    let runtime = match ensure_native_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            ipc_callback_err(&callback, &e);
            return;
        }
    };

    let code_hash = compute_code_hash(&code);
    crate::vprintln!(
        "[NATIVE] Code hash for '{}': {} (trimmed len={})",
        name,
        &code_hash[..16],
        code.trim().len()
    );

    let plugin_prefix = name
        .rsplit_once('/')
        .map(|(p, _)| p)
        .unwrap_or(&name)
        .to_string();
    let manifest_json: String = crate::state::db().call_plugins({
        let prefix = plugin_prefix.clone();
        move |pc| {
            pc.query_row(
                "SELECT manifest FROM plugins WHERE name = ?1 AND installed = 1",
                rusqlite::params![prefix],
                |row| row.get(0),
            )
            .unwrap_or_default()
        }
    });

    let trust_grants: HashMap<String, bool> = {
        let decisions = crate::state::db().call_settings({
            let plugin = name.clone();
            move |conn| crate::native_runtime::trust::load_trust(conn, &plugin)
        });
        decisions
            .into_iter()
            .map(|d| (d.module, d.granted))
            .collect()
    };

    do_register(
        runtime,
        name,
        code,
        code_hash,
        trust_grants,
        manifest_json,
        callback,
    );
}

/// Send register command to Bun, handle TRUST_REQUIRED sentinel.
fn do_register(
    runtime: &'static NativeRuntime,
    name: String,
    code: String,
    code_hash: String,
    mut trust_grants: HashMap<String, bool>,
    manifest_json: String,
    callback: IpcCallback,
) {
    let trust_json: serde_json::Value = if trust_grants.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!(trust_grants)
    };
    let cmd = serde_json::json!({
        "type": "register",
        "name": name,
        "code": code,
        "trust": trust_json,
    });
    let rx = match runtime.send_command(cmd) {
        Ok(rx) => rx,
        Err(e) => {
            ipc_callback_err(&callback, &e);
            return;
        }
    };

    crate::state::rt_handle().spawn(async move {
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
                clear_pending_trust(&name);
                ipc_callback_ok(&callback, &format!("\"{channel}\""));
            }
            Ok(Err(e)) => {
                if let Some(raw) = e.strip_prefix("TRUST_REQUIRED:") {
                    // Trim stack trace — only keep the module name (first line, no whitespace)
                    let module = raw.lines().next().unwrap_or(raw).trim().to_string();

                    if trust_grants.get(&module) == Some(&false) {
                        crate::vprintln!(
                            "[NATIVE] Trust previously denied for '{}' → module '{}'",
                            name,
                            module
                        );
                        ipc_callback_err(
                            &callback,
                            &format!(
                                "Plugin '{}' denied access to module '{}' (persisted)",
                                name, module
                            ),
                        );
                        return;
                    }

                    crate::vprintln!(
                        "[NATIVE] Trust required for '{}' → module '{}'",
                        name,
                        module
                    );

                    // Dedup: if a dialog is already pending for this module,
                    // subscribe to the same watch channel — no duplicate popup.
                    let trust_key = format!("{}::{}", name, module);
                    let mut rx = {
                        let mut guard = PENDING_TRUST.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(existing_tx) = guard.get(&trust_key) {
                            existing_tx.subscribe()
                        } else {
                            let (tx, sub_rx) = watch::channel(None);
                            guard.insert(trust_key.clone(), tx);
                            drop(guard);

                            // Show trust dialog in a separate CEF window.
                            // Isolated from the main renderer — plugins can't interact with it.
                            let dialog_key = trust_key.clone();
                            let dialog_rx = crate::ui::trust_dialog::show_trust_dialog(
                                &name,
                                &module,
                                &manifest_json,
                            );
                            // Broadcast result without removing the key — late
                            // subscribers can still dedup via rx.borrow(). The key
                            // is removed after save_trust makes the decision durable.
                            crate::state::rt_handle().spawn(async move {
                                let granted = dialog_rx.await.unwrap_or(false);
                                let guard = PENDING_TRUST.lock().unwrap_or_else(|e| e.into_inner());
                                if let Some(tx) = guard.get(&dialog_key) {
                                    let _ = tx.send(Some(granted));
                                }
                            });
                            sub_rx
                        }
                    };

                    // Wait for the dialog result.
                    // Check current value first — a late subscriber may find the
                    // answer already set by an earlier concurrent register call.
                    let granted = if let Some(val) = *rx.borrow() {
                        val
                    } else {
                        loop {
                            if rx.changed().await.is_err() {
                                break false;
                            }
                            if let Some(val) = *rx.borrow() {
                                break val;
                            }
                        }
                    };

                    {
                        let hash = code_hash.clone();
                        let plugin = name.clone();
                        let mod_name = module.clone();
                        crate::state::db().call_settings(move |conn| {
                            if let Err(e) = crate::native_runtime::trust::save_trust(
                                conn, &hash, &plugin, &mod_name, granted,
                            ) {
                                crate::vprintln!(
                                    "[NATIVE] Failed to save trust for {}::{}: {}",
                                    plugin,
                                    mod_name,
                                    e
                                );
                            }
                        });
                    }
                    // DB is durable — safe to remove the dedup key now.
                    {
                        let mut guard = PENDING_TRUST.lock().unwrap_or_else(|e| e.into_inner());
                        guard.remove(&trust_key);
                    }

                    if !granted {
                        crate::vprintln!(
                            "[NATIVE] Trust denied for '{}' → module '{}'",
                            name,
                            module
                        );
                        ipc_callback_err(
                            &callback,
                            &format!("Plugin '{}' denied access to module '{}'", name, module),
                        );
                        return;
                    }

                    trust_grants.insert(module, true);
                    do_register(
                        runtime,
                        name,
                        code,
                        code_hash,
                        trust_grants,
                        manifest_json,
                        callback,
                    );
                    return;
                }

                crate::vprintln!("[NATIVE] Register failed for '{}': {}", name, e);
                ipc_callback_err(&callback, &e);
            }
            Err(_) => {
                ipc_callback_err(&callback, "Bun response channel dropped");
            }
        }
    });
}

/// Handle `__LunaNative.{name}` IPC calls to a registered native module.
pub(super) fn handle_native_call(msg: &IpcMessage, callback: IpcCallback) {
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

    let Some(runtime) = NATIVE_RUNTIME.get() else {
        ipc_callback_err(&callback, "Native runtime not initialized");
        return;
    };

    let cmd = serde_json::json!({
        "type": "call",
        "name": module_name,
        "fn": export_name,
        "args": call_args,
    });
    let rx = match runtime.send_command(cmd) {
        Ok(rx) => rx,
        Err(e) => {
            ipc_callback_err(&callback, &e);
            return;
        }
    };
    crate::state::rt_handle().spawn(async move {
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
}
