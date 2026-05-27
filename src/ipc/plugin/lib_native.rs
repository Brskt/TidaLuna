//! IPC handlers backing the `@luna/lib.native` module exports. Channels are
//! prefixed `__Luna.` so they are routed to `handle_plugin_ipc` with Promise
//! support and gated to trusted frames by `src/ui/client.rs`.

use super::{ipc_callback_err, ipc_callback_ok};
use crate::app_state::{IpcCallback, IpcMessage, eval_js, is_safe_open_url, open_in_os};
use crate::ui::file_dialog::show_file_dialog;
use crate::ui::message_dialog::show_message_dialog;
use cef::FileDialogMode;
use serde_json::{Value, json};

/// `clipboardWriteText(text) -> Promise<void>`
pub(super) fn handle_clipboard_write_text(msg: &IpcMessage, callback: IpcCallback) {
    crate::platform::clipboard::write_text(msg.arg(0).to_string());
    ipc_callback_ok(&callback, "null");
}

/// `openExternal(url) -> Promise<void>` - https-only, rejects otherwise.
pub(super) fn handle_open_external(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg.arg(0);
    if is_safe_open_url(url) {
        open_in_os(url);
        ipc_callback_ok(&callback, "null");
    } else {
        ipc_callback_err(
            &callback,
            "openExternal blocked: only https URLs are allowed",
        );
    }
}

/// `sendToRender(channel, ...args) -> Promise<void>` - emits an IPC event to
/// the render frame, consumed by `onIpcEvent` listeners.
pub(super) fn handle_send_to_render(msg: &IpcMessage, callback: IpcCallback) {
    let channel = msg.arg(0);
    if channel.is_empty() {
        ipc_callback_err(&callback, "sendToRender requires a channel name");
        return;
    }
    let args_js = msg
        .args
        .iter()
        .skip(1)
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let channel_js = serde_json::to_string(channel).unwrap_or_else(|_| "\"\"".into());
    let js = if args_js.is_empty() {
        format!(
            "if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__({channel_js});"
        )
    } else {
        format!(
            "if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__({channel_js},{args_js});"
        )
    };
    eval_js(&js);
    ipc_callback_ok(&callback, "null");
}

/// `showMessageBox(options) -> Promise<{ response, checkboxChecked }>`
pub(super) fn handle_show_message_box(msg: &IpcMessage, callback: IpcCallback) {
    let opts = msg.args.first().cloned().unwrap_or(Value::Null);
    let title = opts
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let message = opts
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let detail = opts
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let buttons: Vec<String> = opts
        .get("buttons")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .filter(|v: &Vec<String>| !v.is_empty())
        .unwrap_or_else(|| vec!["OK".to_string()]);
    let default_id = opts.get("defaultId").and_then(Value::as_i64).unwrap_or(-1) as i32;
    // Match Electron: absent cancelId resolves to the first "cancel"/"no" button, else a defaultId-aware seed.
    let cancel_id = match opts.get("cancelId").and_then(Value::as_i64) {
        Some(id) => id as i32,
        None => {
            let mut id = if default_id == 0 && buttons.len() > 1 {
                1
            } else {
                0
            };
            for (i, button) in buttons.iter().enumerate() {
                let text = button.to_lowercase();
                if text == "cancel" || text == "no" {
                    id = i as i32;
                    break;
                }
            }
            id
        }
    };

    let rx = show_message_dialog(&title, &message, &detail, &buttons, default_id, cancel_id);
    crate::state::rt_handle().spawn(async move {
        let response = rx.await.unwrap_or(cancel_id);
        let result = json!({ "response": response, "checkboxChecked": false });
        ipc_callback_ok(&callback, &result.to_string());
    });
}

/// `showErrorBox(title, content) -> Promise<void>`
pub(super) fn handle_show_error_box(msg: &IpcMessage, callback: IpcCallback) {
    let title = msg.arg(0).to_string();
    let content = msg.arg(1).to_string();
    let buttons = vec!["OK".to_string()];

    let rx = show_message_dialog(&title, &content, "", &buttons, 0, 0);
    crate::state::rt_handle().spawn(async move {
        let _ = rx.await;
        ipc_callback_ok(&callback, "null");
    });
}

/// `showOpenDialog(options) -> Promise<{ canceled, filePaths }>`
pub(super) fn handle_show_open_dialog(msg: &IpcMessage, callback: IpcCallback) {
    let opts = msg.args.first().cloned().unwrap_or(Value::Null);
    let title = opts
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string);
    let default_path = opts
        .get("defaultPath")
        .and_then(Value::as_str)
        .map(str::to_string);
    let filters = parse_filters(&opts);

    let properties: Vec<&str> = opts
        .get("properties")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mode = if properties.contains(&"openDirectory") {
        FileDialogMode::OPEN_FOLDER
    } else if properties.contains(&"multiSelections") {
        FileDialogMode::OPEN_MULTIPLE
    } else {
        FileDialogMode::OPEN
    };

    let rx = show_file_dialog(mode, title, default_path, filters);
    crate::state::rt_handle().spawn(async move {
        let paths = rx.await.unwrap_or_default();
        let result = json!({ "canceled": paths.is_empty(), "filePaths": paths });
        ipc_callback_ok(&callback, &result.to_string());
    });
}

/// `showSaveDialog(options) -> Promise<{ canceled, filePath }>`
pub(super) fn handle_show_save_dialog(msg: &IpcMessage, callback: IpcCallback) {
    let opts = msg.args.first().cloned().unwrap_or(Value::Null);
    let title = opts
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string);
    let default_path = opts
        .get("defaultPath")
        .and_then(Value::as_str)
        .map(str::to_string);
    let filters = parse_filters(&opts);

    let rx = show_file_dialog(FileDialogMode::SAVE, title, default_path, filters);
    crate::state::rt_handle().spawn(async move {
        let paths = rx.await.unwrap_or_default();
        let file_path = paths.into_iter().next().unwrap_or_default();
        let result = json!({ "canceled": file_path.is_empty(), "filePath": file_path });
        ipc_callback_ok(&callback, &result.to_string());
    });
}

/// Flatten Electron-style `filters: [{ name, extensions: [...] }]` into the
/// `.ext` entries CEF's `accept_filters` accepts.
fn parse_filters(opts: &Value) -> Vec<String> {
    opts.get("filters")
        .and_then(Value::as_array)
        .map(|filters| {
            filters
                .iter()
                .filter_map(|f| f.get("extensions").and_then(Value::as_array))
                .flatten()
                .filter_map(Value::as_str)
                .filter(|ext| *ext != "*")
                .map(|ext| format!(".{ext}"))
                .collect()
        })
        .unwrap_or_default()
}
