use crate::app_state::{IpcCallback, Ticket, with_state};
use crate::ipc::plugin::ipc_callback_ok;

use super::do_load_plugins_inline;

/// Whether plugin code may run now: a known session, in a page that has not signed out.
pub(in crate::ipc::plugin) fn session_for_plugins() -> bool {
    with_state(|state| state.session_for_plugins()).unwrap_or(false)
}

/// `jsrt.load_plugins`: parks the caller for the pass that will answer it. Answers `true` at once
/// when plugin code may not run (no known session, or a signed-out page).
pub(in crate::ipc::plugin) fn handle_jsrt_load_plugins(callback: IpcCallback) {
    if !session_for_plugins() {
        crate::vprintln!("[PLUGIN] Skipping plugin load: no session, or the page is signed out");
        ipc_callback_ok(&callback, "true");
        return;
    }

    // Parked first, then requested. Parking is what makes a second caller join the pass already
    // running instead of starting its own over the same plugins, and it is the only way its
    // answer can arrive: the pass, not this handler, replies.
    with_state(|state| {
        let ticket = state.ticket();
        state.plugin_load_waiters.push((ticket, callback));
    });

    request_plugin_load();
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
        ipc_callback_ok(cb, "true");
    }
    if let Some(next) = next {
        dispatch_plugin_load(next);
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/ipc/plugin/jsrt/gate.rs"]
mod tests;
