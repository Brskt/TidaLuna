use crate::app_state::{exec_js_on_frame, with_state};
use crate::ui::nav::{self, NavigationPolicy, PageKind};
use cef::*;
use std::cell::Cell;

/// Whether a document commit is the end of the page that owned this host's music.
///
/// A reload, a crash recovery and a navigation all commit a new main document over the one that
/// issued the loads, and the new one boots knowing nothing of what plays. Same-page navigations
/// (history API, fragments) never commit, which `on_load_start` documents. Only the app's own
/// browser and only its main frame count: a subframe commits while the page it belongs to lives
/// on, and an auth popup is not the page that plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DocumentEnd {
    /// The app's main document is gone, and the music it owned is left with nobody.
    Owner,
    /// A commit that replaces nothing that owned the music.
    Other,
}

impl DocumentEnd {
    fn of(in_app_browser: bool, is_main_frame: bool) -> Self {
        match (in_app_browser, is_main_frame) {
            (true, true) => Self::Owner,
            (true, false) | (false, _) => Self::Other,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(in crate::ui) enum PageState {
    Initial,
    App,
    Login,
}

/// One-shot guard for the post-login cold-boot reload (see on_loading_state_change).
/// Reset on session_clear: each login triggers exactly one reload, and a bounce
/// back to the login page can't turn it into a loop.
pub(crate) static POST_LOGIN_RELOADED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// One-shot: armed at boot (finish_boot_tokens) when the SDK blob is unusable
/// (corrupt or no token match). Consumed on the first navigation that injects
/// the init script; the renderer wipes the stale blob exactly once - never
/// re-wiping a blob a later login writes to the same origin. Baking the purge
/// into the reusable init_script instead would replay it on every reload.
pub(crate) static NEEDS_BOOT_BLOB_PURGE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

wrap_load_handler! {
    pub(in crate::ui) struct TidalLoadHandler {
        init_script: String,
        bundle_script: String,
        page_state: Cell<PageState>,
    }
    impl LoadHandler {
        fn on_load_start(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _transition_type: TransitionType,
        ) {
            if let Some(frame) = frame {
                // Before the new document runs anything: its first load must find the player
                // as a fresh page expects it, and nothing the old page queued left to deliver.
                let in_app_browser = browser.as_ref().is_some_and(|b| b.is_popup() == 0);
                if DocumentEnd::of(in_app_browser, frame.is_main() != 0) == DocumentEnd::Owner {
                    crate::vprintln!("[LOAD]   main document replaced, ending its music");
                    let page = with_state(|state| {
                        state.end_page_music();
                        state.end_page_plugins()
                    });
                    // First in the new document: plugin code sent for another page refuses it.
                    if let Some(page) = page {
                        exec_js_on_frame(frame, &crate::ipc::plugin::page_stamp(page));
                    }
                }
                let url_userfree = frame.url();
                let url = crate::ui::token_filter::userfree_to_string(&url_userfree);
                let policy = NavigationPolicy::for_page(PageKind::classify(&url));
                if policy.inject_init_script {
                    // One-shot boot purge of an unusable SDK blob, before the
                    // init script and before TIDAL's JS reads localStorage. The
                    // first boot navigation is desktop.tidal.com (the blob's
                    // origin): it lands once, on the right page.
                    if NEEDS_BOOT_BLOB_PURGE.swap(false, std::sync::atomic::Ordering::AcqRel) {
                        exec_js_on_frame(frame, crate::ipc::plugin::JS_PURGE_SDK_BLOB);
                    }
                    crate::vprintln!(
                        "[LOAD]   on_load_start init_script: {}",
                        crate::util::truncate_str(&crate::util::redact_url_query(&url), 80)
                    );
                    exec_js_on_frame(frame, &self.init_script);
                    // Inject the log level live, not baked into the static init_script
                    // (which a reload replays stale): the player.dbg gate must match
                    // Rust's current effective level.
                    exec_js_on_frame(
                        frame,
                        &format!(
                            "window.__TIDALUNAR_LOG_LEVEL__ = {};",
                            crate::logging::log_level()
                        ),
                    );
                }
            }
        }
        fn on_loading_state_change(
            &self,
            browser: Option<&mut Browser>,
            is_loading: i32,
            _can_go_back: i32,
            _can_go_forward: i32,
        ) {
            crate::vprintln!(
                "[LOAD]   on_loading_state_change: is_loading={} can_go_back={} can_go_forward={}",
                is_loading, _can_go_back, _can_go_forward
            );
            if is_loading == 0
                && let Some(browser) = browser
                && let Some(frame) = browser.main_frame()
            {
                let url_userfree = frame.url();
                let url = crate::ui::token_filter::userfree_to_string(&url_userfree);
                let kind = PageKind::classify(&url);
                let policy = NavigationPolicy::for_page(kind);
                if kind == PageKind::AuthHost {
                    crate::vprintln!(
                        "[LOAD]   Auth page loaded: {}",
                        crate::util::truncate_str(&crate::util::redact_url_query(&url), 100)
                    );
                }
                if !policy.inject_bundle {
                    return;
                }

                let is_login =
                    matches!(kind, PageKind::LoginPage | PageKind::LoginCallback);
                let prev = self.page_state.get();

                // Transitioning from login to app. TIDAL registers its player SDK
                // only during the cold bootstrap on the app route; the SPA
                // login->app transition reaches the app without re-running it, and
                // the play saga then throws "No active player" (the redux activePlayer
                // slice is rehydrated and is not what the saga checks). Reload the
                // app root once to reproduce the known-good cold launch.
                if prev == PageState::Login && !is_login {
                    with_state(|state| {
                        let _ = state.player.stop(crate::player::StopScope::OwnMusic);
                        state.pending_player_events.clear();
                        state.pending_time_update = None;
                    });
                    if !crate::ui::client::POST_LOGIN_RELOADED
                        .swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        crate::vprintln!("[LOAD]   Post-login reload to re-register the player");
                        self.page_state.set(PageState::App);
                        let cef_url = CefString::from(format!("https://{}/", nav::HOST_DESKTOP).as_str());
                        frame.load_url(Some(&cef_url));
                        return;
                    }
                    crate::vprintln!("[LOAD]   Post-login transition to app");
                }

                if prev != PageState::Initial && is_login {
                    with_state(|state| {
                        let _ = state.player.stop(crate::player::StopScope::OwnMusic);
                    });
                }
                self.page_state.set(if is_login {
                    PageState::Login
                } else {
                    PageState::App
                });
                // Reached on every aggregate load completion, subframes included, but inert after
                // the first run in a given document: `bundle_script` carries its own sentinel
                // (see app_bootstrap.rs).
                exec_js_on_frame(&frame, &self.bundle_script);

                // After post-login SPA navigation to app, signal JS to re-run init().
                // Plugin loading is handled by init() -> invokeIpc("jsrt.load_plugins").
                if !is_login && prev == PageState::Login {
                    crate::app_state::emit_ipc_event("jsrt.post_login_init");
                    // Check for updates after login
                    crate::updater::trigger_update_check();
                }

                // Also check on first app load (not coming from login)
                if !is_login && prev == PageState::Initial {
                    crate::updater::trigger_update_check();
                }
            }
        }
        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            let url = failed_url.map(|u| u.to_string()).unwrap_or_default();
            let text = error_text.map(|t| t.to_string()).unwrap_or_default();
            crate::vprintln!(
                "[LOAD]   on_load_error: url={} code={:?} text={}",
                crate::util::redact_url_query(&url),
                error_code,
                text
            );
            let is_main = _frame.as_ref().map(|f| f.is_main() != 0).unwrap_or(false);
            crate::vprintln!("[LOAD]   on_load_error: is_main_frame={}", is_main);
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/ui/client/load.rs"]
mod tests;
