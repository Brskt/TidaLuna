//! A store that moved its address. A plugin's url is where its updates come from: its plugins
//! follow once the user agrees. Only the app's own bundle asks, before any plugin of the process
//! runs. Until the answer they stay out of the load passes, with the plugins depending on them.

use super::admission::Closed;
use super::jsrt::{PassGuard, PassOutcome, announce_stopped};
use super::{ipc_callback_err, ipc_callback_ok};
use crate::app_state::{IpcCallback, IpcMessage, PassStood, emit_ipc_event_with_args, with_state};
use crate::plugins::store::{EnabledPlugin, StoreMove};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, MutexGuard};

/// The plugins a dialog asks about, under their url before the move, to their name. Kept out of
/// the load passes until the answer.
static ASKED: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);

fn asked() -> MutexGuard<'static, HashMap<String, String>> {
    ASKED.lock().unwrap_or_else(|e| e.into_inner())
}

/// One dialog at a time: the next move waits its turn, its plugins held meanwhile.
static ASKING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `plugin.migrate_base_url(from, to)`, answered at once with the number of plugins asked about.
/// Refused once a plugin of the process was admitted, or that plugin could point every other
/// plugin's updates at its own host; checked on the UI thread, ordered with the admission.
pub(super) fn handle_plugin_migrate_base_url(msg: &IpcMessage, callback: IpcCallback) {
    if with_state(|state| state.plugin_code_admitted).unwrap_or(true) {
        ipc_callback_err(
            &callback,
            403,
            "Store moves are asked before any plugin runs",
        );
        return;
    }
    let (from, to) = (msg.arg(0).to_string(), msg.arg(1).to_string());
    crate::state::rt_handle().spawn(async move {
        if from.is_empty() || to.is_empty() {
            ipc_callback_err(&callback, 400, "a store move needs both urls");
            return;
        }
        let read = {
            let (from, to) = (from.clone(), to.clone());
            super::jsrt::in_section(move |_| hold(from, to)).await
        };
        let moves = match read {
            Ok(Ok(moves)) => moves,
            Ok(Err(e)) => return ipc_callback_err(&callback, 500, &format!("DB error: {e}")),
            Err(e) => return ipc_callback_err(&callback, 500, &e),
        };
        ipc_callback_ok(&callback, &moves.len().to_string());
        if !moves.is_empty() {
            ask(from, to, moves).await;
        }
    });
}

/// The plugins this move asks about, held from here on. Run under the pass lock: a pass has
/// either loaded a plugin already, which leaves it out, or reads the hold before loading.
fn hold(from: String, to: String) -> rusqlite::Result<Vec<StoreMove>> {
    let pending = crate::state::db()
        .call_plugins(move |pc| crate::plugins::store::pending_store_move(pc, &from, &to))?;
    let unloaded: Vec<StoreMove> = pending
        .into_iter()
        .filter(|m| is_unloaded(&m.from))
        .collect();
    let mut asking = asked();
    let moves: Vec<StoreMove> = unloaded
        .into_iter()
        .filter(|m| !asking.contains_key(&m.from))
        .collect();
    asking.extend(moves.iter().map(|m| (m.from.clone(), m.name.clone())));
    Ok(moves)
}

fn is_unloaded(url: &str) -> bool {
    with_state(|state| state.plugin_manager.current_load_id(url).is_none()).unwrap_or(false)
}

/// Asks, then carries the answer out under the pass lock.
async fn ask(from: String, to: String, moves: Vec<StoreMove>) {
    let _turn = ASKING.lock().await;
    let answer = crate::ui::store_move_dialog::show_store_move_dialog(&from, &to, &moves)
        .await
        .ok()
        .flatten();
    let settled = super::jsrt::in_section(move |pass| settle(pass, &to, moves, answer)).await;
    if let Err(e) = settled {
        crate::verr!("[PLUGIN] Store move from {from:?} left unanswered: {e}");
    }
}

/// Move re-keys the plugins with their storage and loads them at the new url; Cancel turns them
/// off. No answer (closed, or never shown) loads them where they were, asked again next launch.
/// A plugin the user enabled again meanwhile runs under its old url: it stays there.
fn settle(pass: &PassGuard, to: &str, moves: Vec<StoreMove>, answer: Option<bool>) {
    let db = crate::state::db();
    let (moves, live): (Vec<StoreMove>, Vec<StoreMove>) =
        moves.into_iter().partition(|m| is_unloaded(&m.from));
    let mut resume = Vec::new();
    match answer {
        Some(true) => {
            let applied = {
                let moves = moves.clone();
                db.call_plugins(move |pc| crate::plugins::store::apply_store_move(pc, &moves))
            };
            match applied {
                Ok(()) => {
                    for m in &moves {
                        emit_ipc_event_with_args("jsrt.plugin_moved", &[&m.from, &m.to]);
                        resume.push(m.to.clone());
                    }
                }
                Err(e) => {
                    crate::verr!("[PLUGIN] Store move to {to:?} failed: {e}");
                    resume.extend(moves.iter().map(|m| m.from.clone()));
                }
            }
        }
        Some(false) => {
            let reason = format!("Turned off: its store moved to {to} and the move was declined");
            for m in &moves {
                let url = m.from.clone();
                match db.call_plugins(move |pc| crate::plugins::store::disable(pc, &url)) {
                    Ok(()) => emit_ipc_event_with_args("jsrt.plugin_failed", &[&m.from, &reason]),
                    Err(e) => {
                        crate::verr!("[PLUGIN] Cannot turn {:?} off: {e}", m.from);
                        resume.push(m.from.clone());
                    }
                }
            }
        }
        None => resume.extend(moves.iter().map(|m| m.from.clone())),
    }
    {
        let mut asking = asked();
        for m in moves.iter().chain(&live) {
            asking.remove(&m.from);
        }
    }
    load(pass, resume);
}

/// Loads `urls` and the plugins waiting on the move, the other enabled plugins counting as loaded,
/// which holds once this page's pass tried every plugin; before it, that pass loads them itself.
/// After a pass that could not, they are told so and the next page load takes them.
fn load(pass: &PassGuard, urls: Vec<String>) {
    if !super::jsrt::has_session() {
        return;
    }
    let ready = with_state(|state| {
        let ticket = state.ticket();
        match state.page.pass_stood(ticket) {
            PassStood::NotYet => None,
            stood => Some((ticket, stood, state.page.take_waiting())),
        }
    })
    .flatten();
    let Some((ticket, stood, waiting)) = ready else {
        return;
    };
    let urls: HashSet<String> = urls.into_iter().chain(waiting.into_keys()).collect();
    if urls.is_empty() {
        return;
    }
    let stop = |why: &str| {
        let reason = format!("Not loaded: {why}");
        announce_stopped(
            ticket,
            urls.iter()
                .map(|url| (url.clone(), reason.clone()))
                .collect(),
        );
    };
    if stood == PassStood::Incomplete {
        stop("the load pass could not prepare every plugin");
        return;
    }
    let admission = match super::admission::admit(pass, ticket) {
        Ok(admission) => admission,
        Err(Closed::Stale) => {
            crate::vprintln!("[PLUGIN] Store move's plugins wait: the page or session changed");
            return;
        }
        Err(closed) => {
            crate::verr!(
                "[PLUGIN] Store move's plugins did not load: {}",
                closed.reason()
            );
            stop(closed.reason());
            return;
        }
    };
    let rows = match crate::state::db().call_plugins(crate::plugins::store::collect_enabled_code) {
        Ok(rows) => rows,
        Err(e) => {
            crate::verr!("[PLUGIN] Store move's plugins did not load: {e}");
            stop("the plugin list could not be read");
            return;
        }
    };
    let asking = asked().clone();
    let (todo, loaded): (Vec<EnabledPlugin>, Vec<EnabledPlugin>) = rows
        .into_iter()
        .filter(|p| !asking.contains_key(&p.url))
        .partition(|p| urls.contains(&p.url));
    let loaded = loaded.into_iter().map(|p| p.name).collect();
    let held_names = asking.into_values().collect();
    let outcome = super::jsrt::run_pass(&admission, todo, loaded, &held_names);
    // The next answer must not count as loaded what this one could not prepare.
    if let PassOutcome::HelperFailed(_) = outcome {
        with_state(|state| {
            if state.ticket() == ticket {
                state.page.mark_incomplete(ticket);
            }
        });
    }
    let reason = "Turned off: it could not load once its store move was answered";
    for url in outcome.failed() {
        emit_ipc_event_with_args("jsrt.plugin_failed", &[&url, reason]);
    }
}

/// Splits a pass's rows: the plugins asked about leave it, and their names come back for the
/// plugins depending on them.
pub(super) fn hold_back(rows: Vec<EnabledPlugin>) -> (HashSet<String>, Vec<EnabledPlugin>) {
    let asking = asked();
    let (held, rest): (Vec<_>, Vec<_>) =
        rows.into_iter().partition(|p| asking.contains_key(&p.url));
    (held.into_iter().map(|p| p.name).collect(), rest)
}

/// The plugins among `unresolved` that depend on a `held` name, directly or through another
/// waiting one, url to name.
pub(super) fn waiting_on(
    unresolved: &[EnabledPlugin],
    held: &HashSet<String>,
) -> HashMap<String, String> {
    let deps: Vec<(&EnabledPlugin, Vec<String>)> = unresolved
        .iter()
        .map(|p| (p, dependency_names(&p.manifest)))
        .collect();
    let mut blocked: HashSet<&str> = held.iter().map(String::as_str).collect();
    let mut waiting = HashMap::new();
    loop {
        let mut grew = false;
        for (p, deps) in &deps {
            if !waiting.contains_key(&p.url) && deps.iter().any(|d| blocked.contains(d.as_str())) {
                waiting.insert(p.url.clone(), p.name.clone());
                blocked.insert(&p.name);
                grew = true;
            }
        }
        if !grew {
            return waiting;
        }
    }
}

fn dependency_names(manifest: &str) -> Vec<String> {
    crate::plugins::store::parse_luna_meta(manifest)
        .ok()
        .flatten()
        .map(|meta| meta.dependencies.into_iter().map(|d| d.name).collect())
        .unwrap_or_default()
}

/// The pass's plugins left waiting, loaded once the move they wait on is answered. Kept by the
/// page the pass ran for only: a pass that outlived its page would hand the next one plugins its
/// own pass loads, and the answer would inject them a second time.
pub(super) fn wait_for(ticket: crate::app_state::Ticket, waiting: HashMap<String, String>) {
    with_state(|state| {
        if state.ticket() == ticket {
            state.page.wait(waiting);
        }
    });
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/store_move.rs"]
mod tests;
