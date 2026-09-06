//! Putting plugin code into the renderer, and deciding whether it may stay.
//!
//! Two callers reach this: the boot pass that loads every enabled plugin, and a single
//! `plugin.enable`. Both had the sequence written out longhand, and only one of them re-read
//! the session epoch afterwards; an enable that raced a logout left code running in a
//! session the user had left, with a capability nothing would revoke. Keeping the rule beside
//! each copy of the sequence is what let the second copy be written without it; keeping the
//! sequence in one place is what stops the third.

use crate::app_state::{eval_js, with_state};

/// What became of an injection, and what the caller still owes.
///
/// Every failing variant has already marked the plugin unloaded: that half is the same for
/// both callers. What differs is what each owes on top (the boot pass records a failed url
/// for its batch reconciliation, an enable reverts the `enabled` flag it just set), and that
/// stays with them.
pub(super) enum Injected {
    /// The code is in the renderer and the session that asked for it is still current.
    /// `load_id` identifies this load for the readiness ack and its timeout.
    Live { load_id: u64 },
    /// No randomness available: no nonce and no capability could be minted. Nothing was
    /// transpiled and nothing was dispatched.
    RngUnavailable,
    /// The plugin's own source could not be prepared. Nothing was dispatched.
    TranspileFailed(String),
    /// There was no renderer frame to take the code.
    NoFrame,
    /// The session ended while this was in flight. The injection has been undone.
    SessionChanged,
}

/// What the boot pass owes for an outcome, and whether it may go on at all.
///
/// The pass used to read this off `Injected` through a wildcard, which left `SessionChanged`
/// wearing the same clothes as a plugin that could not load. Reconciliation writes
/// `enabled = 0`. A logout timed against the last plugin of a pass disabled it for every
/// session afterwards, and the plugin was never at fault. Three duties named apart are what
/// keeps a sixth outcome from reopening that: `pass_duty` matches exhaustively, and the
/// compiler asks each new one to choose.
pub(super) enum PassDuty {
    /// The code is live under this pass's own session; record the load and count the progress.
    Record { load_id: u64 },
    /// The attempt ended with nothing live while its session stayed current; the batch
    /// reconciliation owes it a disable.
    Reconcile,
    /// The session this pass ran for is gone. Nothing is owed and nothing may be written:
    /// whatever this pass concluded describes a plugin set the next session never asked about.
    Abandon,
}

impl Injected {
    /// Sort an outcome into the duty it leaves the boot pass.
    ///
    /// `SessionChanged` is the one outcome that says nothing about the plugin. Its injection
    /// has already been undone, and the `enabled` flag it arrived with was set by a user in a
    /// session nothing here has disproved. The updater reached the same conclusion from the
    /// other side, where a check that could not conclude keeps the offer it never disproved.
    pub(super) fn pass_duty(&self) -> PassDuty {
        match self {
            Injected::Live { load_id } => PassDuty::Record { load_id: *load_id },
            Injected::RngUnavailable | Injected::TranspileFailed(_) | Injected::NoFrame => {
                PassDuty::Reconcile
            }
            Injected::SessionChanged => PassDuty::Abandon,
        }
    }
}

/// True when the session that asked for an injection is still the current one.
///
/// An unreadable state answers false. Reading the absence as consent is how an injection
/// outlives the session that asked for it.
pub(super) fn injection_still_current(pass_epoch: u64, current_epoch: Option<u64>) -> bool {
    current_epoch == Some(pass_epoch)
}

/// Mint this load's identity, transpile the plugin, dispatch it, and keep it only if the
/// session that asked for it is still current.
///
/// `epoch` is the session the caller decided to inject under, read before this call. The
/// re-read happens here, after the dispatch, because that is the only point where the answer
/// can have changed: transpiling is real CPU work that holds no lock, and a logout lands on a
/// different thread.
pub(super) fn inject_plugin(url: &str, name: &str, code: &str, epoch: u64) -> Injected {
    let (Some(nonce), Some(capability)) = (
        crate::plugins::manager::random_nonce(),
        crate::plugins::manager::random_capability(),
    ) else {
        // Nothing to retire: `mark_loading` has not run and this attempt owns no load. Clearing
        // the plugin here would reach past this attempt and drop whatever load was already
        // current, which has nothing to do with a failed draw of randomness.
        crate::vprintln!("[PLUGIN] RNG unavailable, skipping '{name}'");
        return Injected::RngUnavailable;
    };

    let load_id = with_state(|state| {
        state
            .plugin_manager
            .mark_loading(url, name, nonce, &capability, epoch)
    })
    .unwrap_or(0);

    let js = match crate::plugins::PluginManager::transpile_and_wrap(
        url,
        code,
        load_id,
        nonce,
        &capability,
    ) {
        Ok(js) => js,
        Err(e) => {
            crate::vprintln!("[PLUGIN] Failed to prepare '{name}': {e}");
            with_state(|state| state.plugin_manager.retire_load(url, load_id));
            return Injected::TranspileFailed(e.to_string());
        }
    };

    crate::vprintln!(
        "[PLUGIN] Prepared '{}' ({} bytes, gen={})",
        name,
        js.len(),
        load_id
    );

    if !eval_js(&js) {
        crate::vprintln!("[PLUGIN] No renderer frame for '{name}'");
        with_state(|state| state.plugin_manager.retire_load(url, load_id));
        return Injected::NoFrame;
    }

    if !injection_still_current(epoch, with_state(|state| state.session_epoch)) {
        // The session ended while this was transpiling, and the sweep that ran meanwhile could
        // not touch this plugin: `mark_loading` had registered it before any code existed, and
        // the cleanup it dispatched was a no-op while `mark_unloaded` then dropped the only
        // record of it. The injection just posted therefore lands in a session the user has
        // left, with nothing left to unload it.
        //
        // Posting the cleanup HERE is what makes it work where the sweep's did not: both leave
        // this thread through the same queue, in program order; this one runs after the
        // injection has executed and has something to undo.
        crate::vprintln!("[PLUGIN] Session changed under '{name}' - undoing its injection");
        undo_injection(url, load_id);
        return Injected::SessionChanged;
    }

    Injected::Live { load_id }
}

/// Undo an injection that was dispatched and then found unwanted, for the callers whose own
/// bookkeeping fails after this module has handed back `Live`.
///
/// `load_id` names the load being undone. The callers all hold it (it is what `Live` handed
/// them), and passing it is what keeps this from reaching past its own attempt into whatever
/// load became current in the meantime.
pub(super) fn undo_injection(url: &str, load_id: u64) {
    let cleanup = crate::plugins::PluginManager::generate_unload_js(url);
    eval_js(&cleanup);
    with_state(|state| state.plugin_manager.retire_load(url, load_id));
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/inject.rs"]
mod tests;
