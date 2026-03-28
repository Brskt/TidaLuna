use super::{ipc_callback_err, ipc_callback_ok};
use crate::app_state::{IpcCallback, IpcMessage, with_state};
use crate::native_runtime::NativeRuntime;
use crate::state::{NATIVE_RUNTIME, NATIVE_RUNTIME_INIT};

/// Initialize the native runtime (Bun child process) if not already running.
///
/// Uses double-check locking: fast path is a single atomic load on `OnceLock::get()`.
/// Slow path serializes via `NATIVE_RUNTIME_INIT` mutex to prevent double spawn.
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
    let rt_handle = with_state(|state| state.rt_handle.clone())
        .ok_or_else(|| "AppState not initialized".to_string())?;
    let rt = NativeRuntime::spawn(&rt_handle)
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

    let cmd = serde_json::json!({
        "type": "register",
        "name": name,
        "code": code,
    });
    let rx = match runtime.send_command(cmd) {
        Ok(rx) => rx,
        Err(e) => {
            ipc_callback_err(&callback, &e);
            return;
        }
    };
    let Some(rt) = with_state(|state| state.rt_handle.clone()) else {
        return;
    };

    rt.spawn(async move {
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

/// Handle `__LunaNative.{name}` IPC calls to a registered native module.
///
/// INVARIANT: `registerNative` must be called before any `__LunaNative.*` call.
/// The JS plugin loader guarantees this — registerNative is synchronous in the
/// plugin load path, and native calls only happen after the module is registered.
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
    let Some(rt) = with_state(|state| state.rt_handle.clone()) else {
        return;
    };

    rt.spawn(async move {
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
