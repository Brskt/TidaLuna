use super::admission::{Admission, Closed};
use crate::app_state::{IpcMessage, Ticket, emit_ipc_event_with_args, eval_js, with_state};
use cef::*;
use std::collections::{HashMap, HashSet};

/// Purges the real tokens of the SDK's localStorage blob before plugin code runs. The SDK already
/// holds them in memory (credentialsProvider), and the blob would be readable by any plugin.
pub(crate) const JS_PURGE_SDK_BLOB: &str = "try{['Data','Counter','Salt','Key'].forEach(function(s){localStorage.removeItem('AuthDB/tidal'+s)})}catch(e){}";

// Soft clear: token only. TIDAL calls this during "not logged in" flow -
// must not destroy cookies/localStorage/sessionStorage.

fn handle_session_clear() {
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
/// Runs off the CEF UI thread, one pass at a time, under the ticket it was dispatched with. It
/// re-reads that ticket before touching each plugin: a logout or a reload runs on the UI thread
/// and cannot stop this pass. The pass has to notice and stop itself.
pub(super) fn do_load_plugins_inline(ticket: Ticket) {
    exclusive(|pass| {
        // Before any row is read, plugins or not: the admission is also what purges the blob.
        let admission = match super::admission::admit(pass, ticket) {
            Ok(admission) => admission,
            Err(Closed::Stale) => {
                crate::vprintln!("[PLUGIN] Load pass dropped: the page or session changed");
                return;
            }
            Err(closed) => {
                crate::verr!("[PLUGIN] Load pass did not run: {}", closed.reason());
                stop_all(ticket, &format!("Not loaded: {}", closed.reason()));
                return;
            }
        };
        let db = crate::state::db();

        // 1. Dedup same-name plugins (legacy/corruption cleanup)
        let deduped = db.call_plugins(crate::plugins::store::dedup_same_name);
        for (url, name) in &deduped {
            crate::vprintln!("[PLUGIN] Startup dedup: removed '{name}' ({url})");
        }

        // 2. Collect all enabled plugins with code + manifest, less those whose store move waits
        // for the user's answer
        let enabled = match db.call_plugins(crate::plugins::store::collect_enabled_code) {
            Ok(enabled) => enabled,
            Err(e) => {
                crate::verr!("[PLUGIN] Load pass could not read the enabled plugins: {e}");
                stop_all(ticket, "Not loaded: the plugin list could not be read");
                return;
            }
        };
        let (held, remaining) = super::store_move::hold_back(enabled);
        let outcome = run_pass(&admission, remaining, HashSet::new(), &held);
        // Still under the pass lock: a store move's answer sees either a pass that has not read
        // its rows yet or one that is done and recorded.
        with_state(|state| {
            if state.ticket() == ticket {
                match outcome {
                    PassOutcome::Completed(_) => state.page.mark_loaded(ticket),
                    PassOutcome::HelperFailed(_) => state.page.mark_incomplete(ticket),
                    PassOutcome::Abandoned => {}
                }
            }
        });
    });
}

/// A pass that could not run leaves every enabled plugin off this page, flag kept: each card is
/// told, and the page no longer counts as loaded. Best effort: the rows are read again here, and a
/// plugin whose own row refuses it already shows why.
fn stop_all(ticket: Ticket, reason: &str) {
    match crate::state::db().call_plugins(crate::plugins::store::collect_enabled_code) {
        Ok(rows) => announce_stopped(
            ticket,
            rows.into_iter()
                .filter(|p| crate::plugins::store::load_refusal(&p.name, &p.manifest).is_none())
                .map(|p| (p.url, reason.to_string()))
                .collect(),
        ),
        Err(e) => crate::verr!("[PLUGIN] Cannot tell the page which plugins stay off: {e}"),
    }
    with_state(|state| {
        if state.ticket() == ticket {
            state.page.mark_incomplete(ticket);
        }
    });
}

/// Tells the page which plugins stay enabled but not running, flag untouched. Skips a replaced page
/// and any url that has a load, whichever path named it.
pub(super) fn announce_stopped(ticket: Ticket, plugins: Vec<(String, String)>) {
    let stopped = with_state(|state| {
        (state.ticket() == ticket).then(|| {
            plugins
                .into_iter()
                .filter(|(url, _)| state.plugin_manager.current_load_id(url).is_none())
                .collect::<Vec<_>>()
        })
    })
    .flatten()
    .unwrap_or_default();
    for (url, reason) in stopped {
        emit_ipc_event_with_args("jsrt.plugin_stopped", &[url.as_str(), reason.as_str()]);
    }
}

/// The plugins a pass could not resolve, split by what they are owed.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Unresolved {
    /// Stranded by a plugin's own failure: turned off.
    pub(super) failed: Vec<String>,
    /// Stranded once this machine failed to prepare a plugin: flag kept, the page told.
    pub(super) stopped: Vec<String>,
}

/// Splits the plugins left with unresolved dependencies. A machine's failure says nothing about the
/// plugins waiting on what it could not prepare; one waiting on a store move's answer is in
/// neither list, since the answer loads it.
pub(super) fn settle_unresolved(
    unresolved: Vec<String>,
    waiting: &HashMap<String, String>,
    helper_failed: bool,
) -> Unresolved {
    let (stopped, failed) = unresolved
        .into_iter()
        .filter(|url| !waiting.contains_key(url))
        .partition(|_| helper_failed);
    Unresolved { failed, stopped }
}

/// How a load pass ended.
pub(super) enum PassOutcome {
    /// Every plugin was tried; these urls failed and were turned off.
    Completed(Vec<String>),
    /// The machine could not prepare every plugin: these urls failed; the ones it could not prepare
    /// and their dependents stay on, the page told.
    HelperFailed(Vec<String>),
    /// The session or the page changed or closed under the pass.
    Abandoned,
}

impl PassOutcome {
    pub(super) fn failed(self) -> Vec<String> {
        match self {
            PassOutcome::Completed(failed) | PassOutcome::HelperFailed(failed) => failed,
            PassOutcome::Abandoned => Vec::new(),
        }
    }
}

/// Held by every writer of a plugin's row or live load: passes, store moves, enable, disable,
/// install, uninstall, restarts, ack timeouts. Two of them interleaved act on what the other changed.
static PASS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Proof the pass lock is held; only [`exclusive`] makes one.
pub(super) struct PassGuard(());

/// Runs `f` under the pass lock. From a blocking thread only: never the UI thread, the DB actor,
/// nor a tokio worker. Not reentrant.
pub(super) fn exclusive<R>(f: impl FnOnce(&PassGuard) -> R) -> R {
    let _pass = PASS.lock().unwrap_or_else(|e| e.into_inner());
    f(&PassGuard(()))
}

/// [`exclusive`] on the blocking pool, for async callers.
pub(super) async fn in_section<R: Send + 'static>(
    f: impl FnOnce(&PassGuard) -> R + Send + 'static,
) -> Result<R, String> {
    tokio::task::spawn_blocking(move || exclusive(f))
        .await
        .map_err(|e| format!("spawn_blocking failed: {e}"))
}

/// Loads `remaining` in dependency order, `loaded_names` counting as loaded already, and answers
/// how it ended with the urls it gave up on and disabled. `held` names the plugins waiting on a
/// store move: a plugin depending on one waits with it instead of failing.
pub(super) fn run_pass(
    admission: &Admission,
    mut remaining: Vec<crate::plugins::store::EnabledPlugin>,
    mut loaded_names: HashSet<String>,
    held: &HashSet<String>,
) -> PassOutcome {
    let ticket = admission.ticket();
    let db = crate::state::db();
    let mut failed_urls = Vec::new();
    let mut helper_failed = false;
    // Left on and not running, with the reason each card shows.
    let mut stopped: Vec<(String, String)> = Vec::new();

    // 3. Multi-pass: load plugins whose deps are satisfied
    let mut dispatched_snapshot: Vec<(String, u64)> = Vec::new(); // (url, load_id) for timeout

    loop {
        let mut progress = false;
        let mut still_remaining = Vec::new();

        for p in remaining {
            // Checked per plugin, not once per pass: `retire_all_loaded` sweeps the manager
            // on the UI thread and has no way to interrupt this loop. Injecting after that sweep
            // would resurrect a plugin into a session or page that is gone, with nothing left
            // behind to unload it again.
            if with_state(|state| state.ticket()) != Some(ticket) {
                crate::vprintln!("[PLUGIN] Load pass abandoned: the page or session changed");
                return PassOutcome::Abandoned;
            }
            // Checked here rather than when the row is read: that read skips a row it cannot
            // decode without a word, and a refused name has to reach the reconciliation below.
            let name = match crate::plugins::name::PluginName::parse(&p.name) {
                Ok(name) => name,
                Err(refused) => {
                    crate::vprintln!("[PLUGIN] Skipping {}: {refused}", p.url);
                    failed_urls.push(p.url);
                    continue;
                }
            };
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
                        // No failing outcome leaves a load of this attempt registered. What is
                        // left is this pass's own due: a session that ended under an injection
                        // is no failed plugin, and reconciling it would disable a working one.
                        match super::inject::inject_plugin(admission, &p.url, &name, &p.code) {
                            // Its own load set the flag and is watched already.
                            Ok(super::inject::Injected::AlreadyLive) => {
                                loaded_names.insert(p.name);
                                progress = true;
                            }
                            Ok(super::inject::Injected::Started(load_id)) => {
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
                            Err(failure) => match failure.pass_duty() {
                                super::inject::PassDuty::Reconcile => failed_urls.push(p.url),
                                super::inject::PassDuty::Keep => {
                                    helper_failed = true;
                                    stopped
                                        .push((p.url, format!("Not loaded: {}", failure.reason())));
                                }
                                super::inject::PassDuty::Abandon => {
                                    crate::vprintln!(
                                        "[PLUGIN] Load pass abandoned: the page or session changed or closed under '{}'",
                                        p.name
                                    );
                                    return PassOutcome::Abandoned;
                                }
                            },
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

    // 4. Log + reconcile: plugins with unresolved deps. Not once the helper failed: a dependency
    // it could not prepare says nothing about the plugin waiting on it. Nor while a dependency
    // waits on a store move: the answer loads the plugin.
    let waiting = super::store_move::waiting_on(&remaining, held);
    for p in &remaining {
        if waiting.contains_key(&p.url) {
            crate::vprintln!("[PLUGIN] '{}' waits on a store move's answer", p.name);
        } else {
            crate::vprintln!("[PLUGIN] Cannot load '{}': unresolved dependencies", p.name);
        }
    }
    let unresolved = settle_unresolved(
        remaining.into_iter().map(|p| p.url).collect(),
        &waiting,
        helper_failed,
    );
    failed_urls.extend(unresolved.failed);
    stopped.extend(unresolved.stopped.into_iter().map(|url| {
        (
            url,
            "Not loaded: the load pass could not prepare every plugin".to_string(),
        )
    }));
    super::store_move::wait_for(ticket, waiting);

    // 5. Reconciliation: auto-disable plugins that couldn't load
    if !failed_urls.is_empty() {
        let urls = failed_urls.clone();
        db.call_plugins(move |pc| {
            for url in &urls {
                if let Err(e) = crate::plugins::store::disable_failed(pc, url) {
                    crate::vprintln!("[PLUGIN] Failed to auto-disable '{url}': {e}");
                } else {
                    // Not gated: the user did not ask for this, and the flag stays off on disk.
                    // Debug-quoted, since the url comes from the plugin's own row.
                    crate::verr!("[PLUGIN] Auto-disabled {url:?}: failed to load");
                }
            }
        });
    }

    crate::vprintln!(
        "[PLUGIN] Startup complete: {} loaded, {} failed",
        loaded_names.len(),
        failed_urls.len()
    );

    announce_stopped(ticket, stopped);

    // 6. Startup timeout: each dispatched plugin's ready ack, watched on its own
    for (url, load_id) in dispatched_snapshot {
        crate::state::rt_handle().spawn(super::inject::watch_ack(url, load_id, ticket));
    }
    if helper_failed {
        PassOutcome::HelperFailed(failed_urls)
    } else {
        PassOutcome::Completed(failed_urls)
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
/// of its own ticket settles.
pub(super) fn request_plugin_load() {
    let start = with_state(|state| {
        if state.plugin_load_in_flight.is_some() {
            return None;
        }
        let ticket = state.ticket();
        state.plugin_load_in_flight = Some(ticket);
        Some(ticket)
    })
    .flatten();
    if let Some(ticket) = start {
        dispatch_plugin_load(ticket);
    }
}

/// Run one pass off the UI thread, then settle it. Only ever called with the in-flight slot held.
fn dispatch_plugin_load(ticket: Ticket) {
    crate::state::rt_handle().spawn(async move {
        // The pass is a long stretch of blocking database work; it belongs on the blocking
        // pool rather than a worker that other futures share.
        if let Err(e) = tokio::task::spawn_blocking(move || do_load_plugins_inline(ticket)).await {
            crate::verr!("[PLUGIN] Load pass did not run: {e}");
        }
        settle_plugin_load(ticket);
    });
}

/// What a settling pass owes, in order: the waiters it may answer, the waiters it may not, and
/// the tag owed a pass of its own.
type SettleSplit<K, T> = (Vec<(K, T)>, Vec<(K, T)>, Option<K>);

/// Split parked waiters into the ones a pass of `tag` may answer and the ones it may not, and
/// name the tag owed a pass of its own.
///
/// Whoever asked after the session or page changed carries a newer tag: this pass read a plugin
/// set they never saw, and the reply is a bare boolean that gives them no way to detect the
/// substitution. Generic, to keep the rule testable without a live IPC callback.
fn settle_split<K: Copy + PartialEq, T>(parked: Vec<(K, T)>, tag: K) -> SettleSplit<K, T> {
    let (answer, rest): (Vec<_>, Vec<_>) =
        parked.into_iter().partition(|(parked, _)| *parked == tag);
    let next = rest.first().map(|(parked, _)| *parked);
    (answer, rest, next)
}

/// Answer the waiters this pass ran for, then start another if a newer ticket is still waiting.
fn settle_plugin_load(ticket: Ticket) {
    let (answer, next) = with_state(|state| {
        state.plugin_load_in_flight = None;
        let (answer, rest, next) =
            settle_split(std::mem::take(&mut state.plugin_load_waiters), ticket);
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
        for (url, load_id) in &ended.loaded {
            eval_js(&crate::plugins::PluginManager::generate_unload_js(
                url, *load_id,
            ));
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
