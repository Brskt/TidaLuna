use super::{ipc_callback_err, ipc_callback_ok};
use crate::app_state::{IpcCallback, IpcMessage, with_state};

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
}

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
}
