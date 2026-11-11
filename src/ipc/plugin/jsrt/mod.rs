use super::admission::{Admission, Closed};
use crate::app_state::{IpcMessage, Ticket, emit_ipc_event_with_args, with_state};
use std::collections::{HashMap, HashSet};

mod gate;
mod session;

use session::{handle_session_clear, handle_session_hard_reset};

pub(crate) use gate::open_plugin_gate;
pub(super) use gate::{handle_jsrt_load_plugins, has_session};

/// Purges the real tokens of the SDK's localStorage blob before plugin code runs. The SDK already
/// holds them in memory (credentialsProvider), and the blob would be readable by any plugin.
pub(crate) const JS_PURGE_SDK_BLOB: &str = "try{['Data','Counter','Salt','Key'].forEach(function(s){localStorage.removeItem('AuthDB/tidal'+s)})}catch(e){}";

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

/// Held by every writer of a plugin's row or live load except the UI thread's sweeps (logout, page
/// end): passes, store moves, enable, disable, install, uninstall, restarts, ack timeouts. Two of
/// them interleaved act on what the other changed.
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

#[cfg(test)]
#[path = "../../../../tests/unit/ipc/plugin/jsrt.rs"]
mod jsrt_tests;
