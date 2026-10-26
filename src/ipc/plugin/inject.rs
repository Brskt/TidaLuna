//! Putting plugin code into the renderer, and deciding whether it may stay.
//!
//! Two callers reach this: the boot pass that loads every enabled plugin, and a single
//! `plugin.enable`. Both had the sequence written out longhand, and only one of them re-read
//! the session epoch afterwards; an enable that raced a logout left code running in a
//! session the user had left, with a capability nothing would revoke. Keeping the rule beside
//! each copy of the sequence is what let the second copy be written without it; keeping the
//! sequence in one place is what stops the third.

use crate::app_state::{eval_js, with_state};

/// Why an injection left nothing live, and what the caller still owes.
///
/// Every variant has already marked the plugin unloaded: that half is the same for every
/// caller. What differs is what each owes on top (the boot pass records a failed url for its
/// batch reconciliation, an enable reverts the `enabled` flag it just set), and that stays with
/// them. A live injection is not one of these: `inject_plugin` hands its `load_id` back as `Ok`.
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
    /// The session ended while this was in flight. The injection has been undone.
    SessionChanged,
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
    /// The session this pass ran for is gone. Nothing is owed and nothing may be written:
    /// whatever this pass concluded describes a plugin set the next session never asked about.
    Abandon,
    /// Nothing went live, and the failure was this machine's, not the plugin's: its flag stays as
    /// the user set it, and so do those of the plugins waiting on it.
    Keep,
}

impl InjectFailure {
    /// Sort a failure into the duty it leaves the boot pass.
    ///
    /// `SessionChanged` is the one failure that says nothing about the plugin. Its injection
    /// has already been undone, and the `enabled` flag it arrived with was set by a user in a
    /// session nothing here has disproved. The updater reached the same conclusion from the
    /// other side, where a check that could not conclude keeps the offer it never disproved.
    pub(super) fn pass_duty(&self) -> PassDuty {
        match self {
            InjectFailure::RngUnavailable
            | InjectFailure::TranspileFailed(_)
            | InjectFailure::NoFrame => PassDuty::Reconcile,
            InjectFailure::SessionChanged => PassDuty::Abandon,
            InjectFailure::HelperUnavailable(_) => PassDuty::Keep,
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
/// session that asked for it is still current. `Ok` is the live load's `load_id`, which keys
/// its readiness ack and the ack's timeout.
///
/// `epoch` is the session the caller decided to inject under, read before this call. The
/// re-read happens here, after the dispatch, because that is the only point where the answer
/// can have changed: transpiling is real CPU work that holds no lock, and a logout lands on a
/// different thread.
pub(super) fn inject_plugin(
    url: &str,
    name: &crate::plugins::name::PluginName,
    code: &str,
    epoch: u64,
) -> Result<u64, InjectFailure> {
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

    if !eval_js(&js) {
        crate::vprintln!("[PLUGIN] No renderer frame for '{name}'");
        with_state(|state| state.plugin_manager.retire_load(url, load_id));
        return Err(InjectFailure::NoFrame);
    }

    if !injection_still_current(epoch, with_state(|state| state.session_epoch)) {
        // The session ended while this was being prepared. A sweep after `mark_loading` named
        // this load, and the page holds its unload against the code; one before never saw it,
        // and nothing else would unload what was just posted.
        crate::vprintln!("[PLUGIN] Session changed under '{name}' - undoing its injection");
        undo_injection(url, load_id);
        return Err(InjectFailure::SessionChanged);
    }

    Ok(load_id)
}

/// Wait on the ready ack of `load_id`, and give the load up once it has gone idle: unload it,
/// persist the disable, tell the page. One task per load: a load paused on its user's dialogs
/// holds back no other plugin's.
pub(super) async fn watch_ack(url: String, load_id: u64) {
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
    let url_disable = url.clone();
    let _ = tokio::task::spawn_blocking(move || {
        crate::state::db().call_plugins(move |pc| {
            let _ = crate::plugins::store::disable_failed(pc, &url_disable);
        });
    })
    .await;
    crate::app_state::emit_ipc_event_with_args("jsrt.plugin_failed", &[&url]);
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
