use crate::app_state::{eval_js, with_state};
use crate::ipc::plugin::{ipc_callback_ok, reset_pkce_scrub};
use cef::*;

use super::JS_PURGE_SDK_BLOB;

// Soft clear: token only. TIDAL calls this during "not logged in" flow -
// must not destroy cookies/localStorage/sessionStorage.

pub(super) fn handle_session_clear() {
    crate::vprintln!("[AUTH]   session_clear received");

    let ended = with_state(|state| {
        // Stop the native player; it runs independent of the renderer: a
        // session clear alone won't halt audio on logout. This account's music only: a cast
        // runs on its controller's credentials and goes on without this login.
        let _ = state.player.stop(crate::player::StopScope::OwnMusic);
        state.pending_player_events.clear();
        state.pending_time_update = None;
        // Re-open the plugin-load gate; the prior cold-boot refresh is moot now.
        state.proactive_refresh_done = true;
        state.end_session()
    })
    .unwrap_or_default();
    settle_ended_session(&ended);
    // Allow the next login to trigger its one-shot cold-boot reload.
    crate::ui::POST_LOGIN_RELOADED.store(false, std::sync::atomic::Ordering::SeqCst);
    let data_dir = crate::state::cache_data_dir();
    // Queued on the same channel as the saves: an erase that overtook a save still in flight
    // would hand the credential back to the next launch after the user logged out.
    crate::platform::secure_store::delete_queued(&data_dir);
    crate::app_state::eval_js(JS_PURGE_SDK_BLOB);
    reset_pkce_scrub();
    crate::vprintln!("[AUTH]   Cleared captured token + token state + SDK auth blob");

    crate::app_state::emit_ipc_event("jsrt.session_cleared");
    crate::vprintln!("[AUTH]   Notified frontend: session_cleared");
}

// Aggressive cleanup: cookies, storage, token. Used for debug/manual logout only.

static HARD_RESET_IN_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(super) fn handle_session_hard_reset() {
    if HARD_RESET_IN_PROGRESS.swap(true, std::sync::atomic::Ordering::SeqCst) {
        crate::vprintln!("[AUTH]   hard_reset already in progress, ignoring");
        return;
    }
    crate::vprintln!("[AUTH]   hard_reset received");

    // The same transition as the soft clear, through the same owner: a pass in flight
    // describes the session being destroyed, and the plugins it loaded outlive the storage
    // wipe below unless something retires them.
    let ended = with_state(|state| state.end_session()).unwrap_or_default();
    settle_ended_session(&ended);
    let data_dir = crate::state::cache_data_dir();
    // Queued on the same channel as the saves: an erase that overtook a save still in flight
    // would hand the credential back to the next launch after the user logged out.
    crate::platform::secure_store::delete_queued(&data_dir);
    reset_pkce_scrub();
    crate::vprintln!("[AUTH]   Cleared captured token + token state");

    if let Some(cm) = cef::cookie_manager_get_global_manager(None) {
        let empty = cef::CefString::from("");
        let mut cb = HardResetCookiesCallback::new(0);
        let ok = cm.delete_cookies(Some(&empty), Some(&empty), Some(&mut cb));
        if ok != 0 {
            crate::vprintln!("[AUTH]   delete_cookies started (async)");
        } else {
            crate::vprintln!("[AUTH]   delete_cookies failed synchronously");
            hard_reset_finalize();
        }
    } else {
        crate::vprintln!("[AUTH]   Cookie manager not available");
        hard_reset_finalize();
    }
}

fn hard_reset_finalize() {
    eval_js("try { localStorage.clear(); sessionStorage.clear(); } catch(e) {}");
    crate::vprintln!("[AUTH]   Cleared web storage");

    crate::app_state::emit_ipc_event("jsrt.session_hard_reset_done");
    crate::vprintln!("[AUTH]   hard_reset complete");

    HARD_RESET_IN_PROGRESS.store(false, std::sync::atomic::Ordering::SeqCst);
}

cef::wrap_delete_cookies_callback! {
    struct HardResetCookiesCallback {
        _p: u8,
    }
    impl DeleteCookiesCallback {
        fn on_complete(&self, num_deleted: ::std::os::raw::c_int) {
            crate::vprintln!("[AUTH]   delete_cookies complete ({} deleted)", num_deleted);
            if let Some(cm) = cef::cookie_manager_get_global_manager(None) {
                let mut cb = HardResetFlushCallback::new(0);
                let ok = cm.flush_store(Some(&mut cb));
                if ok != 0 {
                    crate::vprintln!("[AUTH]   flush_store started (async)");
                } else {
                    crate::vprintln!("[AUTH]   flush_store failed synchronously");
                    hard_reset_finalize();
                }
            } else {
                hard_reset_finalize();
            }
        }
    }
}

cef::wrap_completion_callback! {
    struct HardResetFlushCallback {
        _p: u8,
    }
    impl CompletionCallback {
        fn on_complete(&self) {
            crate::vprintln!("[AUTH]   flush_store complete");
            hard_reset_finalize();
        }
    }
}

/// Tell the page about a session the state has already ended: unload the plugins it had
/// live, and answer the load requests it parked.
///
/// Both reach the renderer; both run once the lock is back. Nothing here can be raced by a load
/// pass: the entries these names came from are retired, the epoch that admitted them is gone,
/// and a pass arriving now is stopped by its own re-check.
fn settle_ended_session(ended: &crate::app_state::EndedSession) {
    if !ended.loaded.is_empty() {
        crate::vprintln!(
            "[PLUGIN] Unloading {} user plugin(s) (session ended)",
            ended.loaded.len()
        );
        for (url, load_id) in &ended.loaded {
            eval_js(&crate::plugins::PluginManager::generate_unload_js(
                url, *load_id,
            ));
        }
    }
    // Letting the renderer's invokeIpc resolve rather than hang across the transition.
    for (_, cb) in &ended.waiters {
        ipc_callback_ok(cb, "true");
    }
}
