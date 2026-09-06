use crate::app_state::{IpcMessage, eval_js, with_state};
use cef::*;

pub(crate) const JS_PURGE_SDK_BLOB: &str = "try{['Data','Counter','Salt','Key'].forEach(function(s){localStorage.removeItem('AuthDB/tidal'+s)})}catch(e){}";

/// Purge real tokens from SDK localStorage blob before plugin code runs.
/// TIDAL SDK already holds tokens in memory (credentialsProvider) - the blob
/// is no longer needed and would be readable by plugins that escape the IIFE.
pub(super) fn purge_sdk_auth_blob_if_needed() {
    let needs = with_state(|state| {
        let n = state.needs_blob_purge;
        state.needs_blob_purge = false;
        n
    })
    .unwrap_or(false);
    if needs {
        eval_js(JS_PURGE_SDK_BLOB);
        crate::vprintln!("[AUTH]   Purged SDK auth blob from localStorage (pre-plugin)");
    }
}

// Soft clear: token only. TIDAL calls this during "not logged in" flow -
// must not destroy cookies/localStorage/sessionStorage.

fn handle_session_clear() {
    crate::vprintln!("[AUTH]   session_clear received");

    let ended = with_state(|state| {
        // Stop the native player; it runs independent of the renderer: a
        // session clear alone won't halt audio on logout.
        let _ = state.player.stop(crate::player::LoadOrigin::Local);
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
    super::reset_pkce_scrub();
    crate::vprintln!("[AUTH]   Cleared captured token + token state + SDK auth blob");

    crate::app_state::emit_ipc_event("jsrt.session_cleared");
    crate::vprintln!("[AUTH]   Notified frontend: session_cleared");
}

// Aggressive cleanup: cookies, storage, token. Used for debug/manual logout only.

static HARD_RESET_IN_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn handle_session_hard_reset() {
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
    super::reset_pkce_scrub();
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

/// Multi-pass plugin loading with dependency ordering and startup reconciliation.
/// Used by both the fire-and-forget path and the request-response handler.
///
/// Runs off the CEF UI thread, one pass at a time, under the session epoch it was dispatched
/// with. It re-reads that epoch before touching each plugin: a logout runs on the UI thread and
/// cannot stop this pass. The pass has to notice and stop itself.
pub(super) fn do_load_plugins_inline(epoch: u64) {
    let db = crate::state::db();

    // 1. Dedup same-name plugins (legacy/corruption cleanup)
    let deduped = db.call_plugins(crate::plugins::store::dedup_same_name);
    for (url, name) in &deduped {
        crate::vprintln!("[PLUGIN] Startup dedup: removed '{name}' ({url})");
    }

    // 2. Collect all enabled plugins with code + manifest
    let mut remaining: Vec<crate::plugins::store::EnabledPlugin> =
        db.call_plugins(crate::plugins::store::collect_enabled_code);
    let mut loaded_names = std::collections::HashSet::new();
    let mut failed_urls = Vec::new();

    // 3. Multi-pass: load plugins whose deps are satisfied
    let mut dispatched_snapshot: Vec<(String, u64)> = Vec::new(); // (url, load_id) for timeout

    loop {
        let mut progress = false;
        let mut still_remaining = Vec::new();

        for p in remaining {
            // Checked per plugin, not once per pass: `unload_all_user_plugins` sweeps the manager
            // on the UI thread and has no way to interrupt this loop. Injecting after that sweep
            // would resurrect a plugin into a session the user has already left, with nothing
            // left behind to unload it again.
            if with_state(|state| state.session_epoch) != Some(epoch) {
                crate::vprintln!("[PLUGIN] Load pass abandoned: the session changed under it");
                return;
            }
            match crate::plugins::store::parse_luna_meta(&p.manifest) {
                Err(msg) => {
                    crate::vprintln!("[PLUGIN] Skipping '{}': invalid manifest: {msg}", p.name);
                    failed_urls.push(p.url);
                }
                Ok(meta) => {
                    let deps = meta
                        .as_ref()
                        .map(|m| &m.dependencies[..])
                        .unwrap_or_default();
                    let deps_satisfied = deps.iter().all(|d| loaded_names.contains(&d.name));

                    if deps_satisfied {
                        // Every failing outcome has already announced itself and marked the
                        // plugin unloaded. What is left is what only this pass owes, and the
                        // three duties are not interchangeable: a session that ended under an
                        // injection is not a plugin that failed to load, and reconciling on it
                        // would disable something nothing was wrong with.
                        let outcome = super::inject::inject_plugin(&p.url, &p.name, &p.code, epoch);
                        match outcome.pass_duty() {
                            super::inject::PassDuty::Record { load_id } => {
                                // Critical: persist ever_dispatched flag
                                let url_flag = p.url.clone();
                                let flag_ok = db.call_plugins(move |pc| {
                                    crate::plugins::store::mark_ever_dispatched(pc, &url_flag)
                                });
                                if flag_ok.is_ok() {
                                    dispatched_snapshot.push((p.url.clone(), load_id));
                                    loaded_names.insert(p.name);
                                    progress = true;
                                } else {
                                    // Flag failed; revert to avoid inconsistent state
                                    crate::vprintln!(
                                        "[PLUGIN] Failed to persist ever_dispatched for '{}', reverting",
                                        p.name
                                    );
                                    super::inject::undo_injection(&p.url, load_id);
                                    failed_urls.push(p.url);
                                }
                            }
                            super::inject::PassDuty::Reconcile => failed_urls.push(p.url),
                            super::inject::PassDuty::Abandon => {
                                crate::vprintln!(
                                    "[PLUGIN] Load pass abandoned: the session changed under '{}'",
                                    p.name
                                );
                                return;
                            }
                        }
                    } else {
                        still_remaining.push(p);
                    }
                }
            }
        }

        remaining = still_remaining;
        if !progress || remaining.is_empty() {
            break;
        }
    }

    // 4. Log + reconcile: plugins with unresolved deps
    for p in &remaining {
        crate::vprintln!("[PLUGIN] Cannot load '{}': unresolved dependencies", p.name);
        failed_urls.push(p.url.clone());
    }

    // 5. Reconciliation: auto-disable plugins that couldn't load
    if !failed_urls.is_empty() {
        let urls = failed_urls.clone();
        db.call_plugins(move |pc| {
            for url in &urls {
                if let Err(e) = crate::plugins::store::disable(pc, url) {
                    crate::vprintln!("[PLUGIN] Failed to auto-disable '{url}': {e}");
                } else {
                    crate::vprintln!("[PLUGIN] Auto-disabled '{url}': failed to load at startup");
                }
            }
        });
    }

    crate::vprintln!(
        "[PLUGIN] Startup complete: {} loaded, {} failed",
        loaded_names.len(),
        failed_urls.len()
    );

    // 6. Startup timeout: check dispatched plugins for ready ack after 10s
    if !dispatched_snapshot.is_empty() {
        crate::state::rt_handle().spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            for (url, load_id) in &dispatched_snapshot {
                // Tested and removed under one borrow. Read first and removed second, the ack
                // this watchdog exists to wait for can land between the two, and the cleanup
                // below would then tear down a plugin that had just succeeded.
                let abandoned = with_state(|state| {
                    state.plugin_manager.abandon_if_still_loading(url, *load_id)
                })
                .unwrap_or(false);
                if abandoned {
                    crate::vprintln!(
                        "[PLUGIN] Startup timeout: '{}' (gen={}) never ready - marking failed",
                        url,
                        load_id
                    );
                    let cleanup_js = crate::plugins::PluginManager::generate_unload_js(url);
                    eval_js(&cleanup_js);
                    let url_disable = url.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        crate::state::db().call_plugins(move |pc| {
                            let _ = crate::plugins::store::disable(pc, &url_disable);
                        });
                    })
                    .await;
                    crate::app_state::emit_ipc_event_with_args("jsrt.plugin_failed", &[url]);
                }
            }
        });
    }
}

/// Open the gate and drain parked load requests. Posts to the CEF UI thread
/// because the drain touches CEF handles that are only sound there.
pub(crate) fn open_plugin_gate() {
    let mut task = GateDrainTask::new(0);
    post_task(ThreadId::UI, Some(&mut task));
}

/// One-shot latch: only the first parked request arms the safety timer.
static GATE_TIMEOUT_ARMED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Force the gate open after 5s if the refresh never signals; plugin load
/// can't hang forever. Safe: the egress filter still blocks any leaked token.
pub(super) fn arm_gate_timeout() {
    if GATE_TIMEOUT_ARMED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    crate::state::rt_handle().spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let still_closed = with_state(|state| !state.proactive_refresh_done).unwrap_or(false);
        if still_closed {
            crate::vprintln!("[PLUGIN] Gate timeout: refresh never signalled, loading anyway");
            open_plugin_gate();
        }
    });
}

wrap_task! {
    struct GateDrainTask {
        _p: u8,
    }
    impl Task {
        fn execute(&self) {
            // Opening the gate no longer runs the pass here. It only releases whoever is parked
            // into the single-flight queue, which is also what a request arriving after the gate
            // opened goes through. One path for both; neither can start a second pass.
            let parked = with_state(|state| {
                state.proactive_refresh_done = true;
                state.plugin_load_waiters.len()
            })
            .unwrap_or(0);
            if parked == 0 {
                return;
            }
            crate::vprintln!("[PLUGIN] Gate open: releasing {parked} parked load request(s)");
            request_plugin_load();
        }
    }
}

/// Start a load pass unless one is already running.
///
/// The waiters list is the queue. A caller parks its callback, then calls this: whether it
/// starts the pass or joins one already running, its answer arrives the same way, when a pass
/// of its own epoch settles.
pub(super) fn request_plugin_load() {
    let start = with_state(|state| {
        if state.plugin_load_in_flight.is_some() {
            return None;
        }
        let epoch = state.session_epoch;
        state.plugin_load_in_flight = Some(epoch);
        Some(epoch)
    })
    .flatten();
    if let Some(epoch) = start {
        dispatch_plugin_load(epoch);
    }
}

/// Run one pass off the UI thread, then settle it. Only ever called with the in-flight slot held.
fn dispatch_plugin_load(epoch: u64) {
    crate::state::rt_handle().spawn(async move {
        purge_sdk_auth_blob_if_needed();
        // The pass is a long stretch of blocking database work; it belongs on the blocking
        // pool rather than a worker that other futures share.
        if let Err(e) = tokio::task::spawn_blocking(move || do_load_plugins_inline(epoch)).await {
            crate::verr!("[PLUGIN] Load pass did not run: {e}");
        }
        settle_plugin_load(epoch);
    });
}

/// What a settling pass owes, in order: the waiters it may answer, the waiters it may not, and
/// the epoch owed a pass of its own.
type SettleSplit<T> = (Vec<(u64, T)>, Vec<(u64, T)>, Option<u64>);

/// Split parked waiters into the ones a pass of `epoch` may answer and the ones it may not, and
/// name the epoch owed a pass of its own.
///
/// Whoever asked after the session changed carries a newer tag: this pass read a plugin set they
/// never saw, and the reply is a bare boolean that gives them no way to detect the substitution.
/// Generic over the payload, to keep the rule testable without a live IPC callback.
fn settle_split<T>(parked: Vec<(u64, T)>, epoch: u64) -> SettleSplit<T> {
    let (answer, rest): (Vec<_>, Vec<_>) =
        parked.into_iter().partition(|(parked, _)| *parked == epoch);
    let next = rest.first().map(|(parked, _)| *parked);
    (answer, rest, next)
}

/// Answer the waiters this pass ran for, then start another if a newer epoch is still waiting.
fn settle_plugin_load(epoch: u64) {
    let (answer, next) = with_state(|state| {
        state.plugin_load_in_flight = None;
        let (answer, rest, next) =
            settle_split(std::mem::take(&mut state.plugin_load_waiters), epoch);
        state.plugin_load_waiters = rest;
        if let Some(next) = next {
            state.plugin_load_in_flight = Some(next);
        }
        (answer, next)
    })
    .unwrap_or_default();

    for (_, cb) in &answer {
        super::ipc_callback_ok(cb, "true");
    }
    if let Some(next) = next {
        dispatch_plugin_load(next);
    }
}

pub(crate) fn handle_jsrt_fire_and_forget(msg: &IpcMessage) {
    match msg.channel.as_str() {
        "jsrt.set_token" => {
            let token = msg.arg(0);
            if crate::ui::token_filter::is_opaque(token) {
                crate::vprintln!("[AUTH]   Ignoring opaque token from renderer");
            } else {
                with_state(|state| {
                    state.captured_token = token.to_string();
                });
                super::scrub_pkce_verifier();
                crate::vprintln!("[PLUGIN] Token captured ({} chars)", token.len());
            }
        }
        "jsrt.session_clear" => {
            handle_session_clear();
        }
        "jsrt.session_hard_reset" => {
            // Destructive: wipes cookies, storage, token. Debug-only guard.
            if crate::logging::log_level() >= 1 {
                handle_session_hard_reset();
            }
        }
        "jsrt.plugin_ready" => {
            let url = msg.arg(0);
            let load_id: u64 = msg
                .args
                .get(1)
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let nonce: u64 = msg
                .args
                .get(2)
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if !url.is_empty() {
                let accepted =
                    with_state(|state| state.plugin_manager.mark_ready(url, load_id, nonce))
                        .unwrap_or(false);
                if accepted {
                    crate::vprintln!("[PLUGIN] Ready: {} (gen={})", url, load_id);
                    crate::app_state::emit_ipc_event_with_args(
                        "jsrt.plugin_ready_confirmed",
                        &[url],
                    );
                } else {
                    crate::vprintln!(
                        "[PLUGIN] Stale ready ack ignored: {} (gen={})",
                        url,
                        load_id
                    );
                }
            }
        }
        _ => {
            crate::vprintln!("[JSRT] Unknown fire-and-forget channel: {}", msg.channel);
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
        for (url, _) in &ended.loaded {
            eval_js(&crate::plugins::PluginManager::generate_unload_js(url));
        }
    }
    // Letting the renderer's invokeIpc resolve rather than hang across the transition.
    for (_, cb) in &ended.waiters {
        super::ipc_callback_ok(cb, "true");
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/jsrt.rs"]
mod jsrt_tests;
