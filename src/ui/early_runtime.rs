//! The early-runtime scripts, carried from the browser process to the renderer.
//!
//! They run synchronously in `on_context_created`, before any TIDAL script, in a renderer not
//! trusted to read files; CEF's `extra_info` carries them. Without the payload the page must not
//! run (token and network guards): a compiled-in stub stops it and the browser refuses to go on.

use std::sync::OnceLock;

use cef::*;

use crate::frontend::load::Frontend;
use crate::ui::nav::PageKind;

pub(crate) const KEY_BODY: &str = "lunar.early_runtime.body";
pub(crate) const KEY_TITLEBAR: &str = "lunar.early_runtime.titlebar";

/// What the renderer tells the browser when a qualifying page had no payload to run.
pub(crate) const EARLY_RUNTIME_MISSING: &str = "lunar.early_runtime_missing";

/// Runs in place of the payload when it is missing. Compiled in on purpose: it is what stands
/// between a broken delivery and a page running without its guards.
pub(crate) const FAIL_CLOSED_JS: &str =
    "try{window.stop();}catch(e){}location.replace('about:blank');";

/// The page kinds whose browsers carry the payload: everything the main view navigates to, and
/// the AuthHost popup, the only popup the app lets open.
pub(crate) const COVERED_KINDS: [PageKind; 4] = [
    PageKind::DesktopApp,
    PageKind::LoginCallback,
    PageKind::LoginPage,
    PageKind::AuthHost,
];

pub(crate) struct EarlyRuntimePayload {
    pub body: String,
    pub titlebar: String,
}

/// The renderer config, then the guard scripts in load order. `init_static`
/// and `cache_heal` stay out: the browser runs them, and the credentials travel with them.
pub(crate) fn payload_from_frontend(f: &Frontend) -> EarlyRuntimePayload {
    let e = &f.early;
    let body = [
        crate::frontend::prologue::renderer_config_js().as_str(),
        &e.host_modules,
        &e.saga_put,
        &e.ipc,
        &e.volume_restore,
        &e.token_capture,
        &e.fetch_proxy,
        &e.window_open,
        &e.session_stub,
        &e.exfil_guard,
        &e.plugin_loads,
    ]
    .join("\n");
    EarlyRuntimePayload {
        body,
        titlebar: e.fallback_titlebar.to_string(),
    }
}

/// Popups get no fallback titlebar. Its buttons drive the main window, and the auth popup is
/// OS-framed anyway.
pub(crate) fn preload_script(p: &EarlyRuntimePayload, is_popup: bool) -> String {
    let titlebar = if is_popup { "" } else { p.titlebar.as_str() };
    format!(
        "(function(){{if(self.__LUNAR_EARLY_RUNTIME__)return;if(typeof window.cefQuery!=='function')return;\n{}\n{}\nself.__LUNAR_EARLY_RUNTIME__=true;}})();",
        p.body, titlebar
    )
}

/// The one builder both senders call (`browser_view_create`, `on_before_popup`); neither can
/// create a browser without the payload.
pub(crate) fn extra_info() -> Option<DictionaryValue> {
    let Some(front) = crate::frontend::load::get() else {
        crate::verr!("[EARLY]  No front loaded: the renderer gets no early runtime");
        return None;
    };
    let payload = payload_from_frontend(front);
    let dict = dictionary_value_create()?;
    let body_ok = dict.set_string(
        Some(&CefString::from(KEY_BODY)),
        Some(&CefString::from(payload.body.as_str())),
    );
    let titlebar_ok = dict.set_string(
        Some(&CefString::from(KEY_TITLEBAR)),
        Some(&CefString::from(payload.titlebar.as_str())),
    );
    if body_ok == 0 || titlebar_ok == 0 {
        crate::verr!("[EARLY]  Could not fill the early-runtime dictionary");
        return None;
    }
    Some(dict)
}

/// Renderer side: kept in a static, since `render_process_handler()` rebuilds the handler struct.
static RECEIVED: OnceLock<EarlyRuntimePayload> = OnceLock::new();

/// Renderer side, from `on_browser_created`. Every browser of one renderer carries the same
/// payload; the first one seeds the static.
pub(crate) fn receive(extra_info: Option<&mut DictionaryValue>) {
    let Some(dict) = extra_info else {
        return;
    };
    let body_key = CefString::from(KEY_BODY);
    let titlebar_key = CefString::from(KEY_TITLEBAR);
    if dict.has_key(Some(&body_key)) == 0 || dict.has_key(Some(&titlebar_key)) == 0 {
        return;
    }
    let body = crate::ui::token_filter::userfree_to_string(&dict.string(Some(&body_key)));
    let titlebar = crate::ui::token_filter::userfree_to_string(&dict.string(Some(&titlebar_key)));
    let _ = RECEIVED.set(EarlyRuntimePayload { body, titlebar });
    crate::vprintln2!(
        "[EARLY]  Payload received ({} bytes)",
        RECEIVED.get().map_or(0, |p| p.body.len())
    );
}

pub(crate) fn received() -> Option<&'static EarlyRuntimePayload> {
    RECEIVED.get()
}

#[cfg(test)]
#[path = "../../tests/unit/ui/early_runtime.rs"]
mod tests;
