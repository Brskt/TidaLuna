//! IPC handlers backing the `@luna/lib.native` module exports. Channels are
//! prefixed `__Luna.`: routed to `handle_plugin_ipc` with Promise support and
//! gated to trusted frames by `src/ui/client.rs`.

use super::{ipc_callback_err, ipc_callback_ok};
use crate::app_state::{IpcCallback, IpcMessage, eval_js, is_safe_open_url, open_in_os};
use crate::ui::destination_dialog::{Native, Request, choose_destination, classify};
use crate::ui::file_dialog::{show_file_dialog, vet_folder};
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
            400,
            "openExternal blocked: only https URLs are allowed",
        );
    }
}

/// `sendToRender(channel, ...args) -> Promise<void>` - emits an IPC event to
/// the render frame, consumed by `onIpcEvent` listeners.
pub(super) fn handle_send_to_render(msg: &IpcMessage, callback: IpcCallback) {
    let channel = msg.arg(0);
    if channel.is_empty() {
        ipc_callback_err(&callback, 400, "sendToRender requires a channel name");
        return;
    }
    let js = build_send_to_render_js(channel, msg.args.get(1..).unwrap_or(&[]));
    eval_js(&js);
    ipc_callback_ok(&callback, "null");
}

/// Build the `__LUNAR_IPC_EMIT__(channel, ...args)` call. Channel and args are
/// JSON-encoded with U+2028/U+2029 escaped: a value can't break out of its literal.
fn build_send_to_render_js(channel: &str, args: &[Value]) -> String {
    let args_js = args
        .iter()
        .map(js_value_literal)
        .collect::<Vec<_>>()
        .join(",");
    let channel_js = crate::app_state::js_string_literal(channel);
    let sep = if args_js.is_empty() { "" } else { "," };
    format!(
        "if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__({channel_js}{sep}{args_js});"
    )
}

/// Serialize a JSON value as a JS expression, escaping U+2028/U+2029.
fn js_value_literal(v: &Value) -> String {
    crate::app_state::escape_js_line_terminators(v.to_string())
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
    let wants_directory = asked_for_a_directory(&properties);
    let mode = if wants_directory {
        FileDialogMode::OPEN_FOLDER
    } else if properties.contains(&"multiSelections") {
        FileDialogMode::OPEN_MULTIPLE
    } else {
        FileDialogMode::OPEN
    };

    crate::state::rt_handle().spawn(async move {
        // Only a folder grants writes; a dialog opened to read a file is answered as is.
        let paths = if wants_directory {
            let guard = crate::platform::install_guard::get();
            let request = Request {
                mode,
                title,
                default_path,
                filters,
            };
            choose_destination(&Native, request, move |answer| {
                classify(answer, |dir| vet_folder_answer(guard, dir))
            })
            .await
        } else {
            show_file_dialog(mode, title, default_path, filters)
                .await
                .unwrap_or_default()
        };
        // A picked directory authorises the per-track writes an album makes into it, which no save
        // dialog ever sees. Taken from the dialog's own answer; the renderer cannot name the folder
        // itself, and off the async worker since canonicalising could block on a stalled mount.
        if wants_directory {
            let granted = paths.clone();
            let protected = tokio::task::spawn_blocking(move || {
                granted.iter().any(|path| {
                    crate::ui::file_dialog::record_folder_grant(std::path::Path::new(path))
                        == Err(crate::ui::file_dialog::NotRecorded::Protected)
                })
            })
            .await
            .unwrap_or(false);
            // Refused here only if the location changed since the dialog judged it.
            if protected {
                ipc_callback_err(&callback, 403, PROTECTED_ANSWER);
                return;
            }
        }
        let result = json!({ "canceled": paths.is_empty(), "filePaths": paths });
        ipc_callback_ok(&callback, &result.to_string());
    });
}

/// Did the caller ask for a directory? This and nothing else mints a write grant: a dialog opened
/// to read a file leaves no permission behind.
fn asked_for_a_directory(properties: &[&str]) -> bool {
    properties.contains(&"openDirectory")
}

/// Does this `defaultPath` name a directory rather than a file? Electron's contract allows a bare
/// directory, meaning "open the dialog here with no suggested name", and `Path::file_name` ignores a
/// trailing separator; treating one as a file name silently moved the dialog to its parent.
fn names_a_directory(raw: &str) -> bool {
    raw.chars().next_back().is_some_and(std::path::is_separator)
        || std::path::Path::new(raw).is_dir()
}

/// The name to open the dialog on, sanitised BEFORE it opens. What the user confirms is what gets
/// written. Sanitising after left the two different for any title carrying a character POSIX allows
/// and Windows refuses, and the overwrite prompt was then about a file we never wrote. Skipped when
/// the path names a directory: there is no name to clean.
fn suggested_save_name(raw: &str) -> String {
    if names_a_directory(raw) {
        return raw.to_string();
    }
    super::download::sanitized_destination(raw)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| raw.to_string())
}

/// `showSaveDialog(options) -> Promise<{ canceled, filePath }>`
pub(super) fn handle_show_save_dialog(msg: &IpcMessage, callback: IpcCallback) {
    let opts = msg.args.first().cloned().unwrap_or(Value::Null);
    let title = opts
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string);
    let raw_default = opts
        .get("defaultPath")
        .and_then(Value::as_str)
        .map(str::to_string);
    let filters = parse_filters(&opts);

    // Runs off the CEF UI thread (`on_query_str`): a stat on a plugin-chosen path, a dead network
    // mount say, would otherwise freeze repainting for that mount's timeout. `show_file_dialog` posts
    // its own UI-thread task and is safe to call from anywhere.
    crate::state::rt_handle().spawn(async move {
        let default_path = match raw_default {
            None => None,
            Some(raw) => {
                match tokio::task::spawn_blocking(move || suggested_save_name(&raw)).await {
                    Ok(name) => Some(name),
                    Err(e) => {
                        crate::verr!("[DIALOG] Could not prepare the suggested name: {e}");
                        None
                    }
                }
            }
        };
        let guard = crate::platform::install_guard::get();
        let request = Request {
            mode: FileDialogMode::SAVE,
            title,
            default_path,
            filters,
        };
        let paths = choose_destination(&Native, request, move |answer| {
            classify(answer, |path| vet_save_answer(guard, path))
        })
        .await;
        let file_path = paths.into_iter().next().unwrap_or_default();
        // Recorded the way the download will resolve it, sanitised, since recording the raw answer
        // would never match; JS still gets the raw path unchanged. Off the async worker because the
        // existence test and the recording both touch the filesystem.
        if !file_path.is_empty() {
            let answered = file_path.clone();
            let recorded = tokio::task::spawn_blocking(move || record_save_answer(&answered)).await;
            // Refused here only if the location changed since the dialog judged it.
            if matches!(
                recorded,
                Ok(Err(crate::ui::file_dialog::NotRecorded::Protected))
            ) {
                ipc_callback_err(&callback, 403, PROTECTED_ANSWER);
                return;
            }
        }
        let result = json!({ "canceled": file_path.is_empty(), "filePath": file_path });
        ipc_callback_ok(&callback, &result.to_string());
    });
}

/// What a plugin hears when the user picked a path inside the app's own files.
const PROTECTED_ANSWER: &str = "the chosen location is inside TidaLunar's own files";

/// Authorise the download that follows a save dialog's answer, under the sanitised name it will
/// write to.
fn record_save_answer(answered: &str) -> Result<(), crate::ui::file_dialog::NotRecorded> {
    match super::download::sanitized_destination(answered) {
        Ok(dest) => {
            // The OS prompts about replacing only when the file is already there, and
            // only for the name it was given: sanitisation can rename that, and the
            // prompt then said nothing about what gets written.
            let confirmed =
                dest.as_os_str() == std::path::Path::new(answered).as_os_str() && dest.exists();
            crate::ui::file_dialog::record_user_choice(&dest, confirmed)
        }
        Err(e) => {
            crate::verr!("[DIALOG] Unusable destination, download not authorised: {e:#}");
            Err(crate::ui::file_dialog::NotRecorded::Unresolvable)
        }
    }
}

/// The save dialog's own judgement, on the sanitised destination: what `record_save_answer` records
/// and the download writes, never the raw name.
fn vet_save_answer(
    guard: Option<&crate::platform::install_guard::Protected>,
    answered: &std::path::Path,
) -> Result<(), crate::ui::file_dialog::NotRecorded> {
    let dest = super::download::sanitized_destination(&answered.to_string_lossy())
        .map_err(|_| crate::ui::file_dialog::NotRecorded::Unresolvable)?;
    crate::ui::file_dialog::vet_destination(guard, &dest).map(drop)
}

/// The folder dialog's own judgement. Only a protected folder is refused. One that cannot be
/// resolved gets no grant (`record_folder_grant`) and the download into it is what fails; refusing
/// it here would loop the dialog until the user gave up.
fn vet_folder_answer(
    guard: Option<&crate::platform::install_guard::Protected>,
    dir: &std::path::Path,
) -> Result<(), crate::ui::file_dialog::NotRecorded> {
    use crate::ui::file_dialog::NotRecorded;
    match vet_folder(guard, dir) {
        Err(NotRecorded::Protected) => Err(NotRecorded::Protected),
        Ok(_) | Err(NotRecorded::Unresolvable) => Ok(()),
    }
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

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/lib_native.rs"]
mod tests;
