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
                write!(f, "{}...({} chars)", &s[..200], s.len())?;
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
    pub(crate) pending_ipc_callbacks: HashMap<String, IpcCallback>,
    pub(crate) pending_window_save: Option<crate::settings::WindowState>,
    pub(crate) window_save_scheduled: bool,
    #[cfg(target_os = "windows")]
    pub(crate) thumbbar: Option<crate::platform::thumbbar::ThumbBar>,
}

// SAFETY: AppState contains non-Send CEF/OS handles and thread-sensitive fields
// (Browser, OsMediaControls, ThumbBar). The code maintains the invariant that
// these fields are only accessed from the CEF UI thread. The Mutex serializes
// access to shared data, but soundness of Send relies on the human-enforced
// guarantee that thread-sensitive fields are never touched from non-UI threads.
unsafe impl Send for AppState {}

pub(crate) static APP_STATE: std::sync::OnceLock<Arc<Mutex<AppState>>> = std::sync::OnceLock::new();

pub(crate) fn with_state<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut AppState) -> R,
{
    APP_STATE.get().map(|s| {
        let mut guard = s.lock().expect("AppState lock poisoned");
        f(&mut guard)
    })
}

pub(crate) fn exec_js_on_frame(frame: &Frame, js: &str) {
    let code = CefString::from(js);
    let url = CefString::from("");
    frame.execute_java_script(Some(&code), Some(&url), 0);
}

pub(crate) fn eval_js(js: &str) {
    let browser = with_state(|state| state.browser.clone());
    if let Some(Some(browser)) = browser
        && let Some(frame) = browser.main_frame()
    {
        exec_js_on_frame(&frame, js);
    }
}

pub(crate) fn emit_ipc_event(channel: &str) {
    let js = format!(
        "if(typeof window.__LUNAR_IPC_EMIT__==='function')window.__LUNAR_IPC_EMIT__('{}');",
        channel.replace('\'', "\\'")
    );
    eval_js(&js);
}

pub(crate) fn open_in_os(target: impl AsRef<std::ffi::OsStr>) {
    let target = target.as_ref();
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args([
                std::ffi::OsStr::new("/C"),
                std::ffi::OsStr::new("start"),
                std::ffi::OsStr::new(""),
                target,
            ])
            .spawn();
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
