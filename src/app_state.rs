use cef::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub(crate) type IpcCallback = Arc<Mutex<dyn cef::wrapper::message_router::BrowserSideCallback>>;

#[derive(Deserialize, Debug)]
pub(crate) struct IpcMessage {
    pub(crate) channel: String,
    #[serde(default)]
    pub(crate) args: Vec<serde_json::Value>,
    #[serde(default)]
    pub(crate) id: Option<String>,
}

impl std::fmt::Display for IpcMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IpcMessage {{ channel: {:?}, args: [", self.channel)?;
        for (i, arg) in self.args.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            let s = arg.to_string();
            if s.len() > 200 {
                write!(
                    f,
                    "{}...({} chars)",
                    crate::util::truncate_str(&s, 200),
                    s.len()
                )?;
            } else {
                write!(f, "{s}")?;
            }
        }
        write!(f, "]")?;
        if let Some(id) = &self.id {
            write!(f, ", id: {id:?}")?;
        }
        write!(f, " }}")
    }
}

impl IpcMessage {
    pub(crate) fn arg(&self, index: usize) -> &str {
        self.args.get(index).and_then(|v| v.as_str()).unwrap_or("")
    }
}

pub(crate) struct AppState {
    pub(crate) player: Arc<crate::player::Player>,
    pub(crate) pending_time_update: Option<(f64, u32)>,
    pub(crate) pending_player_events: Vec<crate::bridge::PlayerBridgeEvent>,
    pub(crate) pending_misc_js: Vec<String>,
    pub(crate) browser: Option<Browser>,
    pub(crate) flush_scheduled: bool,
    pub(crate) media_controls: Option<crate::platform::media_controls::OsMediaControls>,
    pub(crate) media_duration: Option<f64>,
    pub(crate) plugin_manager: crate::plugins::PluginManager,
    pub(crate) captured_token: String,
    pub(crate) token_state: Option<crate::platform::secure_store::StoredTokenState>,
    pub(crate) pending_ipc_callbacks: HashMap<String, IpcCallback>,
    pub(crate) pending_window_save: Option<crate::settings::WindowState>,
    pub(crate) window_save_scheduled: bool,
    #[cfg(target_os = "windows")]
    pub(crate) thumbbar: Option<crate::platform::thumbbar::ThumbBar>,
    pub(crate) close_to_tray: bool,
    pub(crate) force_quit: bool,
    pub(crate) needs_proactive_refresh: bool,
    pub(crate) needs_blob_purge: bool,
    // Gate: plugin JS must not load until the cold-boot real OAuth token has been
    // rotated to opaque nonces in TIDAL's SDK. Default true (warm boot / no
    // session); a cold boot with a pending refresh closes it.
    pub(crate) proactive_refresh_done: bool,
    // Plugin-load requests parked while the gate is closed; replied on drain.
    pub(crate) plugin_load_waiters: Vec<IpcCallback>,
    pub(crate) last_client_id: String,
    pub(crate) connect: Option<crate::connect::ConnectManager>,
}

// SAFETY: holds non-Send CEF/OS handles (Browser, OsMediaControls, ThumbBar);
// Send is sound only because these are touched solely on the CEF UI thread.
unsafe impl Send for AppState {}

pub(crate) static APP_STATE: std::sync::OnceLock<Arc<Mutex<AppState>>> = std::sync::OnceLock::new();

pub(crate) fn with_state<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut AppState) -> R,
{
    APP_STATE.get().map(|s| {
        // Recover on poison instead of panicking: this lock is taken inside CEF
        // `extern "C"` callbacks, where a panic would be UB across the FFI boundary.
        let mut guard = s.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    })
}

pub(crate) fn exec_js_on_frame(frame: &Frame, js: &str) {
    let code = CefString::from(js);
    let url = CefString::from("");
    frame.execute_java_script(Some(&code), Some(&url), 0);
}

/// Dispatch JS to the renderer's main frame; false if no frame. Targeting the main
/// frame regardless of origin is safe: payloads are injection-safe and secret-free,
/// and subframe-drivable channels are gated upstream.
pub(crate) fn eval_js(js: &str) -> bool {
    let browser = with_state(|state| state.browser.clone());
    if let Some(Some(browser)) = browser
        && let Some(frame) = browser.main_frame()
    {
        exec_js_on_frame(&frame, js);
        true
    } else {
        false
    }
}

const IPC_EMIT_PREFIX: &str =
    "if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__(";

// Escape U+2028/U+2029 (JS line terminators serde leaves raw inside string
// literals) so an embedded value can't terminate the statement.
pub(crate) fn escape_js_line_terminators(s: String) -> String {
    // Fast path: U+2028/U+2029 are vanishingly rare, so avoid two full-string
    // scans + an allocation when neither is present (the normal case).
    if !s.contains(['\u{2028}', '\u{2029}']) {
        return s;
    }
    s.replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

// JSON-encode as a JS string literal so a controlled value can't break out and inject.
pub(crate) fn js_string_literal(s: &str) -> String {
    escape_js_line_terminators(serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string()))
}

/// Build a `__TIDAL_IPC_RESPONSE__(id, null, result)` reply with the
/// renderer-controlled `id` JSON-encoded so it can't break out and inject.
/// `result_json` is already-serialized JSON in value position.
pub(crate) fn js_ipc_response(id: &str, result_json: &str) -> String {
    format!(
        "window.__TIDAL_IPC_RESPONSE__({}, null, {})",
        js_string_literal(id),
        result_json
    )
}

pub(crate) fn emit_ipc_event(channel: &str) {
    let js = format!("{IPC_EMIT_PREFIX}{});", js_string_literal(channel));
    let _ = eval_js(&js);
}

pub(crate) fn emit_ipc_event_with_args(channel: &str, args: &[&str]) {
    let args_js: Vec<String> = args.iter().copied().map(js_string_literal).collect();
    let js = format!(
        "{IPC_EMIT_PREFIX}{},{});",
        js_string_literal(channel),
        args_js.join(",")
    );
    let _ = eval_js(&js);
}

pub(crate) fn emit_ipc_event_with_data(channel: &str, data: &impl serde::Serialize) {
    let json = match serde_json::to_string(data) {
        Ok(j) => escape_js_line_terminators(j),
        Err(_) => return,
    };
    let js = format!("{IPC_EMIT_PREFIX}{},{json});", js_string_literal(channel));
    let _ = eval_js(&js);
}

/// Only allow `https://` URLs to be opened by the OS.
/// Prevents plugins from opening local files, executables, or dangerous protocol handlers.
pub(crate) fn is_safe_open_url(target: &str) -> bool {
    url::Url::parse(target)
        .map(|u| u.scheme() == "https")
        .unwrap_or(false)
}

pub(crate) fn open_in_os(target: impl AsRef<std::ffi::OsStr>) {
    let target = target.as_ref();
    #[cfg(target_os = "windows")]
    {
        // ShellExecuteW, not `cmd /C start`: the latter treats `&` in a URL query
        // string as a command separator, truncating links like `?a=1&token=2`.
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let file: Vec<u16> = target.encode_wide().chain(std::iter::once(0)).collect();
        let verb: Vec<u16> = "open\0".encode_utf16().collect();
        // SAFETY: verb/file are null-terminated UTF-16; the unused pointers are null,
        // which ShellExecuteW documents as "no parameters / default directory".
        unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            );
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(target).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(target).spawn();
    }
}

pub(crate) fn toggle_devtools() {
    let browser = with_state(|state| state.browser.clone());
    if let Some(Some(browser)) = browser
        && let Some(host) = browser.host()
    {
        if host.has_dev_tools() == 1 {
            host.close_dev_tools();
        } else {
            host.show_dev_tools(None, None, None, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{js_ipc_response, js_string_literal};

    #[test]
    fn js_string_literal_escapes_quotes_backslashes_and_newlines() {
        assert_eq!(js_string_literal("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
    }

    #[test]
    fn js_string_literal_escapes_line_and_paragraph_separators() {
        // serde_json leaves U+2028/U+2029 raw (RFC 8259), but they are JS line
        // terminators; \u-escape them so the literal stays valid in any position.
        assert_eq!(js_string_literal("a\u{2028}b"), "\"a\\u2028b\"");
        assert_eq!(js_string_literal("a\u{2029}b"), "\"a\\u2029b\"");
    }

    #[test]
    fn js_ipc_response_keeps_malicious_id_inside_one_string_literal() {
        // The audit payload tries to break out of both quote styles.
        let malicious = "x\");evil();//' or '";
        let js = js_ipc_response(malicious, "[1,2]");
        assert!(js.starts_with("window.__TIDAL_IPC_RESPONSE__("));
        assert!(js.ends_with(", null, [1,2])"));
        // The id argument must be a single JSON string literal that decodes back
        // to the exact input - proving it can never escape into code position.
        let first_arg = js
            .strip_prefix("window.__TIDAL_IPC_RESPONSE__(")
            .and_then(|s| s.strip_suffix(", null, [1,2])"))
            .expect("response structure intact");
        let decoded: String =
            serde_json::from_str(first_arg).expect("first arg is a valid JSON string literal");
        assert_eq!(decoded, malicious);
    }
}
