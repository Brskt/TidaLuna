use crate::app_state::{IpcCallback, Ticket, with_state};
use crate::ipc::plugin::ipc_callback_ok;
use cef::*;

use super::do_load_plugins_inline;

/// jsrt.load_plugins as request-response: runs multi-pass + reconciliation, then responds.
/// Refuses to load plugins when no session is active (login page scenario).
pub(in crate::ipc::plugin) fn has_session() -> bool {
    with_state(|state| !state.captured_token.is_empty() || state.token_state.is_some())
        .unwrap_or(false)
}

pub(in crate::ipc::plugin) fn handle_jsrt_load_plugins(callback: IpcCallback) {
    if !has_session() {
        crate::vprintln!("[PLUGIN] Skipping plugin load - no active session");
        ipc_callback_ok(&callback, "true");
        return;
    }

    // Gate: on a cold boot the real OAuth token still lives in TIDAL's SDK until
    // the proactive refresh rotates it to opaque nonces. Park the reply until the
    // gate opens (refresh done or 5s safety timeout); plugins can't read it.
    // Parked first, then requested. Parking is what makes a second caller join the pass already
    // running instead of starting its own over the same plugins, and it is the only way its
    // answer can arrive: the pass, not this handler, replies.
    let gate_closed = with_state(|state| {
        let ticket = state.ticket();
        state.plugin_load_waiters.push((ticket, callback.clone()));
        !state.proactive_refresh_done
    })
    .unwrap_or(false);

    if gate_closed {
        crate::vprintln!("[PLUGIN] Deferring plugin load until proactive refresh completes");
        arm_gate_timeout();
        return;
    }

    request_plugin_load();
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
        ipc_callback_ok(cb, "true");
    }
    if let Some(next) = next {
        dispatch_plugin_load(next);
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/ipc/plugin/jsrt/gate.rs"]
mod tests;
