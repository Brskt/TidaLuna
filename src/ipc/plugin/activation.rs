use super::jsrt::PassGuard;
use super::{ipc_callback_err, ipc_callback_ok};
use crate::app_state::{IpcCallback, IpcMessage, Ticket, with_state};

/// Enable: deps -> DB enable -> transpile -> eval_js -> persist `ever_dispatched`.
/// Not one transaction (DB write + renderer eval can't be); a crash mid-sequence
/// self-heals on the next load via `do_load_plugins_inline` (re-inject/auto-disable).
pub(super) fn handle_plugin_enable(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg.arg(0).to_string();
    // Taken here, on the UI thread, with the page that asked: read after the awaits below, it
    // would name whatever page replaced that one meanwhile.
    let Some(ticket) = with_state(|state| state.ticket()) else {
        ipc_callback_err(&callback, 500, "State unavailable");
        return;
    };
    crate::state::rt_handle().spawn(async move {
        match do_plugin_enable(url, ticket).await {
            Ok(()) => ipc_callback_ok(&callback, "true"),
            Err(e) => ipc_callback_err(&callback, 500, &e),
        }
    });
}

async fn do_plugin_enable(url: String, ticket: Ticket) -> Result<(), String> {
    super::jsrt::in_section(move |pass| enable_in_section(pass, &url, ticket)).await?
}

/// The whole enable under the pass lock; no other writer of the row or the load lands between
/// its steps. A panic past the mark undoes the load this section marked, named by its id.
fn enable_in_section(pass: &PassGuard, url: &str, ticket: Ticket) -> Result<(), String> {
    // Before the flag is set. A refusal leaves nothing to revert.
    let admission =
        super::admission::admit(pass, ticket).map_err(|closed| closed.reason().to_string())?;
    let marked_before = with_state(|state| state.plugin_manager.last_marked(url)).flatten();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        enable_admitted(&admission, url)
    })) {
        Ok(enabled) => enabled,
        Err(_) => {
            let marked = with_state(|state| state.plugin_manager.last_marked(url)).flatten();
            if let Some(load_id) = marked.filter(|id| Some(*id) != marked_before) {
                super::inject::undo_injection(url, load_id);
            }
            revert_enable(url);
            Err("The plugin could not be enabled".to_string())
        }
    }
}

fn enable_admitted(admission: &super::admission::Admission, url: &str) -> Result<(), String> {
    let db = crate::state::db();

    // 1. Check name + deps, set enabled=1, get code (all DB ops)
    let (plugin_name, code) = {
        let url = url.to_string();
        db.call_plugins(move |pc| {
            // The stored name, the one the startup pass loads the plugin under.
            let crate::plugins::store::StoredPlugin {
                name,
                manifest,
                code,
                ..
            } = crate::plugins::store::stored(pc, &url)
                .map_err(|e| format!("DB error: {e}"))?
                .ok_or_else(|| format!("Plugin not found: {url}"))?;
            let name = crate::plugins::name::PluginName::parse(&name)
                .map_err(|refused| format!("Cannot enable: {refused}"))?;

            // Check dependencies are satisfied
            if let Err(missing) = crate::plugins::store::check_dependencies_satisfied(pc, &manifest)
            {
                return Err(format!(
                    "Cannot enable: deps not satisfied: {}",
                    missing.join(", ")
                ));
            }

            // DB: set enabled=1
            crate::plugins::store::enable(pc, &url).map_err(|e| format!("DB error: {e}"))?;

            Ok((name, code))
        })
    }?;

    // 2. Mark loading, transpile and dispatch, through the one owner of that sequence, under the
    // admission's ticket: `inject_plugin` re-reads it after the dispatch to decide whether the
    // code may stay.
    let load_id = match super::inject::inject_plugin(admission, url, &plugin_name, &code) {
        Ok(super::inject::Injected::Started(load_id)) => load_id,
        // Its own load already set the dispatch flag and watches its ack.
        Ok(super::inject::Injected::AlreadyLive) => return Ok(()),
        // No `InjectFailure` leaves a load of this attempt registered. An enable still owes the
        // `enabled` flag it set, undone only when the plugin itself failed: a reload, a logout, a
        // closing window or this machine's failure says nothing about it; the next pass loads it.
        Err(failure) => {
            if let super::inject::PassDuty::Reconcile = failure.pass_duty() {
                revert_enable(url);
            }
            return Err(failure.reason());
        }
    };

    // 3. Persistent flag: the code was dispatched; the cleanup guard depends on it.
    //    If this fails, revert the activation: better a failed enable than an inconsistent flag.
    let url_flag = url.to_string();
    let flag =
        db.call_plugins(move |pc| crate::plugins::store::mark_ever_dispatched(pc, &url_flag));
    if flag.is_err() {
        crate::vprintln!(
            "[PLUGIN] Failed to persist ever_dispatched for '{}' - reverting activation",
            url
        );
        super::inject::undo_injection(url, load_id);
        revert_enable(url);
        return Err("Failed to persist dispatch flag".to_string());
    }

    // 4. Timeout: no ready ack after ten idle seconds, mark failed + notify frontend
    crate::state::rt_handle().spawn(super::inject::watch_ack(
        url.to_string(),
        load_id,
        admission.ticket(),
    ));

    Ok(())
}

fn revert_enable(url: &str) {
    let url = url.to_string();
    let _ =
        crate::state::db().call_plugins(move |pc| crate::plugins::store::disable_failed(pc, &url));
}

/// Atomic disable: guard dependants -> DB disable -> eval_js cleanup -> mark_unloaded.
pub(super) fn handle_plugin_disable(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg.arg(0).to_string();
    crate::state::rt_handle().spawn(async move {
        match do_plugin_disable(url).await {
            Ok(()) => ipc_callback_ok(&callback, "true"),
            Err(e) => ipc_callback_err(&callback, 500, &e),
        }
    });
}

async fn do_plugin_disable(url: String) -> Result<(), String> {
    super::jsrt::in_section(move |pass| {
        // 1. Guard: check enabled dependants + DB disable
        {
            let url = url.clone();
            crate::state::db().call_plugins(move |pc| {
                let name = crate::plugins::store::get_name_by_url(pc, &url)
                    .ok_or_else(|| format!("Plugin not found: {url}"))?;

                match crate::plugins::store::find_enabled_dependants(pc, &name) {
                    Err(msg) => return Err(msg),
                    Ok(deps) if !deps.is_empty() => {
                        return Err(format!(
                            "Cannot disable '{name}': active dependants: {}",
                            deps.join(", ")
                        ));
                    }
                    _ => {}
                }

                // DB: set enabled=0
                crate::plugins::store::disable(pc, &url).map_err(|e| format!("DB error: {e}"))
            })
        }?;
        // 2. Unload from the renderer the load current now: under the pass lock no enable marks
        // another between the row and here. Nothing loaded is fine; the row was all this owed.
        super::inject::unload_current(pass, &url);
        Ok(())
    })
    .await?
}
