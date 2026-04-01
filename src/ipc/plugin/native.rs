use super::{ipc_callback_err, ipc_callback_ok};
use crate::app_state::{IpcCallback, IpcMessage};
use crate::native_runtime::NativeRuntime;
use crate::state::{NATIVE_RUNTIME, NATIVE_RUNTIME_INIT};

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::watch;

/// Pending trust requests: keyed by "code_hash::plugin::module".
/// Uses watch channels so multiple concurrent register calls for the same
/// plugin can share a single dialog prompt.
type TrustMap = HashMap<String, watch::Sender<Option<bool>>>;

static PENDING_TRUST: Mutex<Option<TrustMap>> = Mutex::new(None);

fn pending_trust_map() -> &'static Mutex<Option<TrustMap>> {
    let mut guard = PENDING_TRUST.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(HashMap::new());
    }
    drop(guard);
    &PENDING_TRUST
}

/// Clear in-memory watch channels for a plugin (called on uninstall).
/// Keys are "name::module", so we remove any key starting with the plugin prefix.
pub(super) fn clear_pending_trust(plugin_prefix: &str) {
    let map = pending_trust_map();
    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(ref mut m) = *guard {
        m.retain(|key, _| !key.starts_with(plugin_prefix));
    }
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

    // Load plugin manifest for author info in trust dialog.
    // Plugin name is "DiscordRPC/discord.native.ts" → URL prefix is "DiscordRPC".
    let plugin_prefix = name.split('/').next().unwrap_or(&name).to_string();
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

    // Load existing trust decisions from DB
    let trust_grants: HashMap<String, bool> = {
        let decisions = crate::state::db().call_settings({
            let hash = code_hash.clone();
            let plugin = name.clone();
            move |conn| crate::native_runtime::trust::load_trust(conn, &hash, &plugin)
        });
        decisions
            .into_iter()
            .map(|d| (d.module, d.granted))
            .collect()
    };

    let trust_json: serde_json::Value = if trust_grants.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!(trust_grants)
    };

    do_register(
        runtime,
        name,
        code,
        code_hash,
        trust_grants,
        trust_json,
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
    trust_json: serde_json::Value,
    manifest_json: String,
    callback: IpcCallback,
) {
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
                // Clean up watch channels for this plugin to prevent stale decisions
                // from being reused if the plugin is updated (different code hash).
                clear_pending_trust(&name);
                ipc_callback_ok(&callback, &format!("\"{channel}\""));
            }
            Ok(Err(e)) => {
                // Check for TRUST_REQUIRED sentinel
                if let Some(raw) = e.strip_prefix("TRUST_REQUIRED:") {
                    // Trim stack trace — only keep the module name (first line, no whitespace)
                    let module = raw.lines().next().unwrap_or(raw).trim().to_string();

                    // Fail fast if this module was previously denied
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

                    // Check if a dialog is already pending for this module.
                    // If so, subscribe to the same watch channel — no duplicate popup.
                    // Dedup key uses name::module (no hash) because concurrent register
                    // calls may have slightly different code (settings extraction trim).
                    let trust_key = format!("{}::{}", name, module);
                    let mut rx = {
                        let map = pending_trust_map();
                        let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
                        let m = guard.as_mut().unwrap();
                        if let Some(existing_tx) = m.get(&trust_key) {
                            // Another register call already showed the dialog — just subscribe
                            existing_tx.subscribe()
                        } else {
                            // First request for this module — create channel and show dialog
                            let (tx, sub_rx) = watch::channel(None);
                            m.insert(trust_key.clone(), tx);
                            drop(guard);

                            // Pass manifest JSON as 4th arg for author info in dialog.
                            // Use eval_js directly because emit_ipc_event_with_args
                            // wraps args in single quotes which breaks JSON.
                            let escaped_name = name.replace('\\', "\\\\").replace('\'', "\\'");
                            let escaped_mod = module.replace('\\', "\\\\").replace('\'', "\\'");
                            let escaped_hash = code_hash.replace('\\', "\\\\").replace('\'', "\\'");
                            let js = format!(
                                "if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__('native.trust_request','{}','{}','{}',{});",
                                escaped_name,
                                escaped_mod,
                                escaped_hash,
                                if manifest_json.is_empty() { "null".to_string() } else { manifest_json.clone() }
                            );
                            crate::app_state::eval_js(&js);
                            sub_rx
                        }
                    };

                    // Wait for the user's decision.
                    // Check current value first — a late subscriber may find the
                    // answer already set by an earlier register call.
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

                    // Save to DB
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

                    // Re-try register with the newly granted module
                    trust_grants.insert(module, true);
                    let updated_trust = serde_json::json!(trust_grants);
                    // Recursive retry — will loop back here if another module is needed
                    do_register(
                        runtime,
                        name,
                        code,
                        code_hash,
                        trust_grants,
                        updated_trust,
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

/// Handle trust dialog response from CEF.
/// Called when user clicks Allow or Deny in the native trust dialog overlay.
pub(super) fn handle_native_trust_response(msg: &IpcMessage, callback: IpcCallback) {
    let plugin = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let module = msg
        .args
        .get(1)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // args[2] = code_hash (not used in dedup key, only for logging)
    let granted = msg.args.get(3).and_then(|v| v.as_bool()).unwrap_or(false);

    // Key must match the dedup key in do_register (name::module, no hash).
    // Don't remove the sender — late subscribers need to see the value.
    let trust_key = format!("{plugin}::{module}");
    let sent = {
        let map = pending_trust_map();
        let guard = map.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .as_ref()
            .and_then(|m| m.get(&trust_key))
            .map(|tx| tx.send(Some(granted)).is_ok())
            .unwrap_or(false)
    };

    if sent {
        crate::vprintln!(
            "[NATIVE] Trust response for '{}::{}': {}",
            plugin,
            module,
            if granted { "ALLOWED" } else { "DENIED" }
        );
    } else {
        crate::vprintln!(
            "[NATIVE] No pending trust request for '{}::{}'",
            plugin,
            module
        );
    }

    ipc_callback_ok(&callback, "true");
}
