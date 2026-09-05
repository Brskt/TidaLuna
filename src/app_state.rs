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
    /// Caller capability, resolved to a `Caller` at ingress. A reserved envelope field, not a
    /// positional argument: `arg(i)` indices stay correct; absent rather than a parse failure for
    /// unattributed callers. Omitted from `Display` below on purpose. It must not reach the log.
    #[serde(default)]
    pub(crate) cap: Option<String>,
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

/// A track length and the track it was measured on. The identity cannot be recovered
/// later: the current-metadata slot has already moved on to the next track.
pub(crate) struct MeasuredDuration {
    /// `None` when the payload named no track, and such a measurement matches nothing: a
    /// length is never lent to another track.
    pub(crate) track_id: Option<String>,
    pub(crate) secs: f64,
}

impl MeasuredDuration {
    /// Mints a measurement, holding the field above to what it claims: a blank id names no
    /// track. Every producer goes through here, and an ingress that skips its own normalizing
    /// still cannot mint an id equal to the next blank one.
    pub(crate) fn new(track_id: Option<String>, secs: f64) -> Self {
        Self {
            track_id: track_id
                .as_deref()
                .and_then(crate::util::metadata::trimmed_non_empty),
            secs,
        }
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
    pub(crate) media_duration: Option<MeasuredDuration>,
    pub(crate) plugin_manager: crate::plugins::PluginManager,
    pub(crate) captured_token: String,
    pub(crate) token_state: Option<crate::platform::secure_store::StoredTokenState>,
    /// Keyed by CEF's own per-query id, never by the `id` the caller sent. Three independent
    /// JS counters feed this map (`fetch.__seq` per plugin load, `@luna/lib`'s bundle-wide
    /// one, the early-runtime's page-global one) and all start at 1; a client-chosen key
    /// collides across plugins, across reloads of one plugin, and across channels.
    pub(crate) pending_ipc_callbacks: HashMap<i64, IpcCallback>,
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
    // Plugin-load requests parked behind the gate or behind a running pass, each tagged with the
    // session epoch it was made under; answered when a pass of that same epoch settles.
    pub(crate) plugin_load_waiters: Vec<(u64, IpcCallback)>,
    // Set to the epoch of the load pass currently running off the UI thread. One at a time: two
    // concurrent passes inject every enabled plugin twice into the same frame, and only one of
    // the two can ever have its ready ack accepted.
    pub(crate) plugin_load_in_flight: Option<u64>,
    // Bumped on every session change, and on nothing else. Work started under an older epoch
    // describes a session the user has left: a plugin-load pass must answer nobody who asked
    // after the change, and a token minted before it must not be committed at all (in memory
    // or to disk) because `captured_token` is the live authorization, not a draft.
    pub(crate) session_epoch: u64,
    pub(crate) last_client_id: String,
    pub(crate) connect: Option<crate::connect::ConnectManager>,
}

/// What the slot becomes when a measurement arrives, stated without an `AppState` behind it.
///
/// An untagged measurement names no track: it can never satisfy `same_track`. Storing one
/// buys nothing and costs whichever tagged length a frame had yet to claim.
fn settle_recorded_duration(
    current: Option<MeasuredDuration>,
    measured: MeasuredDuration,
) -> Option<MeasuredDuration> {
    if measured.track_id.is_none() {
        return current;
    }
    Some(measured)
}

/// What a session that has just ended was still holding, for the caller to settle outside
/// the lock: an `eval_js` reaches the renderer and a parked callback answers an IPC
/// request, and neither belongs under the state mutex.
#[derive(Default)]
pub(crate) struct EndedSession {
    /// The plugins the ended session had live in the page.
    pub(crate) loaded: Vec<(String, u64)>,
    /// The load requests it parked, to be answered rather than left hanging across the
    /// logout/login transition.
    pub(crate) waiters: Vec<(u64, IpcCallback)>,
}

impl AppState {
    /// The one way `media_duration` is written, and it never empties: a payload that carries
    /// no length is not evidence that the last measured one was wrong.
    pub(crate) fn record_measured_duration(&mut self, measured: MeasuredDuration) {
        self.media_duration = settle_recorded_duration(self.media_duration.take(), measured);
    }

    /// End the session now in progress, and name everything it was still holding.
    ///
    /// One critical section, because the pieces only hold together taken at once. A load pass
    /// runs off the UI thread: retiring the plugins first and bumping after leaves a window
    /// where a pass registers too late for the sweep and reads the epoch too early to be
    /// stopped by it, so the injection lands in a session the user has left with nothing naming
    /// it. Clearing the token later leaves the matching window the other way, where
    /// `handle_jsrt_load_plugins` still sees a credential and starts a pass under the NEW epoch.
    pub(crate) fn end_session(&mut self) -> EndedSession {
        // Named and dropped in one call, rather than as each cleanup is dispatched: what
        // makes the bump below safe is that no entry survives it, and a retire interleaved
        // with an `eval_js` would hand the lock back between the two.
        let loaded = self.plugin_manager.retire_all_loaded();
        // A pass still running belongs to the session being cleared. Bumping stops it at its
        // next plugin and keeps its result from ever answering a request made from here on,
        // which would report the departed session's plugin set as the new one's.
        self.session_epoch = self.session_epoch.wrapping_add(1);
        self.captured_token.clear();
        self.token_state = None;
        EndedSession {
            loaded,
            waiters: std::mem::take(&mut self.plugin_load_waiters),
        }
    }
}

// SAFETY: `thumbbar` (Windows only) holds raw `ITaskbarList3` pointers and a
// `Cell`, and it is the sole field that is not already `Send`. Sound because the
// taskbar object is created and used solely on the CEF UI thread. Gated to
// Windows on purpose: elsewhere the auto-impl carries `AppState`, and a field that
// stops being `Send` fails the build rather than being absorbed here.
//
// Nothing else here constrains threading: `browser` is a `RefGuard`, souvlaki's
// `MediaControls` wraps agile WinRT objects or a channel and a `JoinHandle`, and
// `BrowserSideCallback` is declared `Send + Sync`. Work that only calls `eval_js` or answers
// an IPC callback may therefore run off the UI thread, `Frame::execute_java_script` reposting
// itself onto it.
#[cfg(target_os = "windows")]
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
// literals): an embedded value can't terminate the statement.
pub(crate) fn escape_js_line_terminators(s: String) -> String {
    // Fast path: U+2028/U+2029 are vanishingly rare; avoid two full-string
    // scans + an allocation when neither is present (the normal case).
    if !s.contains(['\u{2028}', '\u{2029}']) {
        return s;
    }
    s.replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

// JSON-encode as a JS string literal: a controlled value can't break out and inject.
pub(crate) fn js_string_literal(s: &str) -> String {
    escape_js_line_terminators(serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string()))
}

/// Build a `__TIDAL_IPC_RESPONSE__(id, null, result)` reply with the
/// renderer-controlled `id` JSON-encoded; it can't break out and inject.
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
#[path = "../tests/unit/app_state.rs"]
mod tests;
