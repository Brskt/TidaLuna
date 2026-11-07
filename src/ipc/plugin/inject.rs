//! Putting plugin code into the renderer, and deciding whether it may stay.
//!
//! Callers: the boot pass, `plugin.enable` and an update's restart. The
//! re-read after dispatch lives here once: written beside each caller, one skipped it, and an
//! enable racing a logout left code running in the old session, its capability never revoked.

use super::admission::{Admission, Delivery};
use crate::app_state::{Ticket, eval_js, with_state};

/// Why an injection left nothing live, and what the caller still owes.
///
/// No variant leaves a load of this attempt registered: that half is the same for every
/// caller. What differs is what each owes on top (the boot pass records a failed url for its
/// batch reconciliation, an enable reverts the `enabled` flag it just set), and that stays with
/// them. A live injection is not one of these: `inject_plugin` answers it as `Ok`.
pub(super) enum InjectFailure {
    /// No randomness available: no nonce and no capability could be minted. Nothing was
    /// transpiled and nothing was dispatched.
    RngUnavailable,
    /// The plugin's own source could not be prepared. Nothing was dispatched.
    TranspileFailed(String),
    /// The transpile helper could not run, which says nothing about the plugin. Nothing was
    /// dispatched.
    HelperUnavailable(String),
    /// There was no renderer frame to take the code.
    NoFrame,
    /// The session ended while this was in flight. Nothing of it stays live.
    SessionChanged,
    /// The page this was meant for was replaced while it was in flight. Nothing reached the new
    /// one, whose own load pass takes the plugin.
    PageReplaced,
    /// The UI thread never took the code. Nothing was dispatched.
    Undelivered,
}

/// What the boot pass owes for a failed injection, and whether it may go on at all.
///
/// The pass used to read this off the outcome through a wildcard, which left `SessionChanged`
/// wearing the same clothes as a plugin that could not load. Reconciliation writes
/// `enabled = 0`. A logout timed against the last plugin of a pass disabled it for every
/// session afterwards, and the plugin was never at fault. Three duties named apart are what
/// keeps a sixth failure from reopening that: `pass_duty` matches exhaustively, and the
/// compiler asks each new one to choose.
pub(super) enum PassDuty {
    /// The attempt ended with nothing live while its session stayed current; the batch
    /// reconciliation owes it a disable.
    Reconcile,
    /// The session or page this pass ran for is gone. Nothing is owed and nothing may be written:
    /// whatever this pass concluded describes a plugin set no later page asked about.
    Abandon,
    /// Nothing went live, and the failure was this machine's, not the plugin's: its flag stays as
    /// the user set it, and so do those of the plugins waiting on it.
    Keep,
}

impl InjectFailure {
    /// Sort a failure into the duty it leaves the boot pass.
    ///
    /// `RngUnavailable`, `TranspileFailed` and `NoFrame` are reconciled; every other failure says
    /// nothing about the plugin, and the `enabled` flag a user set stays.
    pub(super) fn pass_duty(&self) -> PassDuty {
        match self {
            InjectFailure::RngUnavailable
            | InjectFailure::TranspileFailed(_)
            | InjectFailure::NoFrame => PassDuty::Reconcile,
            InjectFailure::SessionChanged | InjectFailure::PageReplaced => PassDuty::Abandon,
            InjectFailure::HelperUnavailable(_) | InjectFailure::Undelivered => PassDuty::Keep,
        }
    }

    /// What the page is told, written in this one place: an enable shows it as the plugin's
    /// `loadError`, and the updater's restart quotes it after its own lead-in.
    pub(super) fn reason(&self) -> String {
        match self {
            InjectFailure::RngUnavailable => "RNG unavailable".to_string(),
            InjectFailure::TranspileFailed(e) => format!("Failed to prepare: {e}"),
            InjectFailure::HelperUnavailable(e) => format!("Cannot prepare plugins: {e}"),
            InjectFailure::NoFrame => "No renderer frame available".to_string(),
            InjectFailure::SessionChanged => "Session ended during activation".to_string(),
            InjectFailure::PageReplaced => "The page reloaded during activation".to_string(),
            InjectFailure::Undelivered => "The page did not take the plugin in time".to_string(),
        }
    }
}

/// True when the session or page that asked for an injection is still the current one.
///
/// An unreadable state answers false. Reading the absence as consent is how an injection
/// outlives the session that asked for it.
pub(super) fn injection_still_current<K: PartialEq>(asked: K, current: Option<K>) -> bool {
    current == Some(asked)
}

/// Which of the two moved under a ticket that is no longer current.
fn stale(ticket: Ticket) -> InjectFailure {
    if with_state(|state| state.session_epoch) == Some(ticket.epoch) {
        InjectFailure::PageReplaced
    } else {
        InjectFailure::SessionChanged
    }
}

/// Mint this load's identity, transpile the plugin, dispatch it, and keep it only if the
/// session and page it was admitted for are still current. `Ok` is the live load's `load_id`,
/// which keys its readiness ack and the ack's timeout.
///
/// The last re-read happens after the dispatch: transpiling is real CPU work that holds no lock,
/// and a logout or reload lands on a different thread.
pub(super) fn inject_plugin(
    admission: &Admission,
    url: &str,
    name: &crate::plugins::name::PluginName,
    code: &str,
) -> Result<u64, InjectFailure> {
    let ticket = admission.ticket();
    let (Some(nonce), Some(capability)) = (
        crate::plugins::manager::random_nonce(),
        crate::plugins::manager::random_capability(),
    ) else {
        // Nothing to retire: `mark_loading` has not run and this attempt owns no load. Clearing
        // the plugin here would reach past this attempt and drop whatever load was already
        // current, which has nothing to do with a failed draw of randomness.
        crate::vprintln!("[PLUGIN] RNG unavailable, skipping '{name}'");
        return Err(InjectFailure::RngUnavailable);
    };

    // Checked with the mark, in one look: a reload sweeping the manager between the two would
    // leave this stale load registered, and over a newer load of the same url.
    let marked = with_state(|state| {
        (state.ticket() == ticket).then(|| {
            state
                .plugin_manager
                .mark_loading(url, name, nonce, &capability, ticket.epoch)
        })
    })
    .flatten();
    let Some(load_id) = marked else {
        return Err(stale(ticket));
    };

    let js = match crate::plugins::PluginManager::transpile_and_wrap(
        url,
        code,
        load_id,
        nonce,
        &capability,
    ) {
        Ok(js) => js,
        Err(failure) => {
            // Undone in the page too: `mark_loading` took the record of any instance still running
            // there, and only this id's unload, which reaches older ones, still names it.
            undo_injection(url, load_id);
            return match failure {
                crate::plugins::transpile_helper::Failure::Source(e) => {
                    crate::vprintln!("[PLUGIN] Failed to prepare '{name}': {e}");
                    Err(InjectFailure::TranspileFailed(e))
                }
                crate::plugins::transpile_helper::Failure::Helper(e) => {
                    // Not gated: no plugin can load while this lasts, and nothing else says so.
                    crate::verr!("[PLUGIN] Cannot prepare '{name}': {e}");
                    Err(InjectFailure::HelperUnavailable(e))
                }
            };
        }
    };

    crate::vprintln!(
        "[PLUGIN] Prepared '{}' ({} bytes, gen={})",
        name,
        js.len(),
        load_id
    );

    // Nothing reached the page unless it ran: the load this attempt marked is its own to retire.
    let unsent = match super::admission::eval_plugin(admission, &js) {
        Delivery::Ran => None,
        Delivery::NoFrame => {
            crate::vprintln!("[PLUGIN] No renderer frame for '{name}'");
            Some(InjectFailure::NoFrame)
        }
        Delivery::Stale => Some(stale(ticket)),
        Delivery::Undelivered => {
            crate::verr!("[PLUGIN] The page did not take '{name}'");
            Some(InjectFailure::Undelivered)
        }
    };
    if let Some(failure) = unsent {
        with_state(|state| state.plugin_manager.retire_load(url, load_id));
        return Err(failure);
    }

    if !injection_still_current(ticket, with_state(|state| state.ticket())) {
        // The session or page changed while this was being prepared. A sweep after `mark_loading`
        // named this load, and the page holds its unload against the code; one before never saw
        // it, and nothing else would unload what was just posted.
        crate::vprintln!("[PLUGIN] Session or page changed under '{name}', undoing its injection");
        undo_injection(url, load_id);
        return Err(stale(ticket));
    }

    Ok(load_id)
}

/// Wait on the ready ack of `load_id`, and give the load up once it has gone idle: unload it,
/// persist the disable, tell the page. One task per load: a load paused on its user's dialogs
/// holds back no other plugin's. `ticket` is the admission's the load was injected under.
pub(super) async fn watch_ack(url: String, load_id: u64, ticket: Ticket) {
    use crate::plugins::manager::LoadWatch;
    // The clock started at `mark_loading`, before the transpile and, at boot, before every later
    // plugin of the pass; this first step restarts it.
    with_state(|state| {
        if let Some(progress) = state.plugin_manager.load_progress(&url, load_id) {
            progress.step();
        }
    });
    loop {
        let watch = with_state(|state| {
            state
                .plugin_manager
                .watch_load(&url, load_id, std::time::Instant::now())
        })
        .unwrap_or(LoadWatch::Ended);
        match watch {
            LoadWatch::Look(after) => tokio::time::sleep(after).await,
            LoadWatch::Ended => return,
            LoadWatch::Abandoned => break,
        }
    }
    crate::vprintln!(
        "[PLUGIN] Timeout: '{url}' never sent ready ack (gen={load_id}), marking failed"
    );
    eval_js(&crate::plugins::PluginManager::generate_unload_js(
        &url, load_id,
    ));
    // The verdict on this load is taken; the flag and the event wait for the pass lock, where a
    // page change, a newer load or the user's own disable since would make them wrong.
    let tail = super::jsrt::in_section(move |_| {
        let still_owed = with_state(|state| {
            timeout_owed(
                state.ticket() == ticket,
                state.plugin_manager.last_marked(&url),
                load_id,
            )
        })
        .unwrap_or(false);
        if !still_owed {
            return;
        }
        let url_disable = url.clone();
        let disabled = crate::state::db().call_plugins(move |pc| {
            let enabled = crate::plugins::store::is_enabled(pc, &url_disable).unwrap_or(false);
            enabled && crate::plugins::store::disable_failed(pc, &url_disable).is_ok()
        });
        if disabled {
            crate::app_state::emit_ipc_event_with_args("jsrt.plugin_failed", &[&url]);
        }
    })
    .await;
    if let Err(e) = tail {
        crate::verr!("[PLUGIN] A timed-out plugin was left on: {e}");
    }
}

/// Whether a timed-out load still owes its disable: its page is the current one and no load of the
/// url was marked after it. The row still on is checked with the write.
pub(super) fn timeout_owed(same_page: bool, last_marked: Option<u64>, load_id: u64) -> bool {
    same_page && last_marked == Some(load_id)
}

/// Unload whatever load of `url` is current. Under the pass lock: no load can be marked between
/// the read and the retire.
pub(super) fn unload_current(_pass: &super::jsrt::PassGuard, url: &str) {
    if let Some(load_id) = with_state(|state| state.plugin_manager.current_load_id(url)).flatten() {
        undo_injection(url, load_id);
    }
}

/// Undo an injection that was dispatched and then found unwanted, for the callers whose own
/// bookkeeping fails after this module has handed back a live load.
///
/// `load_id` names the load being undone. The callers all hold it (it is what `Ok` handed
/// them), and passing it is what keeps this from reaching past its own attempt into whatever
/// load became current in the meantime.
pub(super) fn undo_injection(url: &str, load_id: u64) {
    let cleanup = crate::plugins::PluginManager::generate_unload_js(url, load_id);
    eval_js(&cleanup);
    with_state(|state| state.plugin_manager.retire_load(url, load_id));
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/inject.rs"]
mod tests;
