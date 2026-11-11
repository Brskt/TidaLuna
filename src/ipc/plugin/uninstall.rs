use super::jsrt::PassGuard;
use super::{ipc_callback_err, ipc_callback_ok};
use crate::app_state::{IpcCallback, with_state};

/// What an uninstall must clear, typed rather than a bare name: an unknown url produced an empty name,
/// which every clear below reads as "all". An unmatched `plugin.uninstall` wiped every plugin's
/// trust rows, tokens and capabilities.
#[derive(Clone)]
pub(super) enum CleanupScope {
    /// One plugin, by canonical manifest name.
    Plugin(crate::plugins::name::PluginName),
    /// Everything, from `plugin.uninstall_all`.
    All,
}

/// The scope an uninstall leaves behind. No name means nothing matched, and nothing is cleared.
/// Nor does a stored name the rule refuses: startup and enable never load such a plugin (nothing
/// in memory belongs to it), and none can be installed under that name to inherit its rows.
pub(super) fn cleanup_scope_for_uninstall(name: String) -> Option<CleanupScope> {
    crate::plugins::name::PluginName::parse(&name)
        .ok()
        .map(CleanupScope::Plugin)
}

/// `plugin.uninstall` (of `target`) and `plugin.uninstall_all`, as one section under the pass lock.
/// Off the UI thread it arrives on, and off the actor: a section waits on the actor.
pub(super) fn handle_plugin_uninstall(channel: String, target: String, callback: IpcCallback) {
    crate::state::rt_handle().spawn(async move {
        let all = channel == "plugin.uninstall_all";
        let done = super::jsrt::in_section(move |pass| uninstall_in_section(pass, all, &target));
        match done.await.and_then(|done| done) {
            Ok(()) => ipc_callback_ok(&callback, "true"),
            Err(e) => ipc_callback_err(&callback, 500, &e),
        }
    });
}

/// The rows first, leaving the plugin as it was on a refusal or a DB error; the page part runs
/// whatever they said, a row deduplicated away having left its load. Trust and native modules go
/// once the loads are gone, as on a reinstall: a load still running would ask or register again.
fn uninstall_in_section(pass: &PassGuard, all: bool, target: &str) -> Result<(), String> {
    let url = target.to_string();
    let db = crate::state::db();
    let scope = db.call_plugins(move |pc| {
        if all {
            crate::plugins::store::uninstall_all(pc).map_err(|e| e.to_string())?;
            Ok(Some(CleanupScope::All))
        } else {
            let name = crate::plugins::store::get_name_by_url(pc, &url).unwrap_or_default();
            if !name.is_empty() {
                match crate::plugins::store::find_dependants(pc, &name) {
                    Err(msg) => return Err(msg),
                    Ok(deps) if !deps.is_empty() => {
                        return Err(format!(
                            "Cannot uninstall '{name}': required by {}",
                            deps.join(", ")
                        ));
                    }
                    _ => {}
                }
            }
            crate::plugins::store::uninstall(pc, &url).map_err(|e| e.to_string())?;
            Ok(cleanup_scope_for_uninstall(name))
        }
    })?;
    if all {
        let loads = with_state(|state| state.plugin_manager.loaded_loads()).unwrap_or_default();
        for (url, load_id) in loads {
            super::inject::undo_injection(&url, load_id);
        }
    } else {
        super::inject::unload_current(pass, target);
    }
    // Every clear matches on the scope: no name or url stands for every plugin. The uninstall is
    // done; a failed trust reset is reported, not answered as a failed uninstall.
    if let Some(scope) = scope {
        let trust_scope = scope.clone();
        if let Err(e) = db.call_settings(move |sc| super::native::forget_trust(sc, &trust_scope)) {
            crate::verr!("[PLUGIN] Could not clear the trust of an uninstalled plugin: {e}");
        }
        super::native::clear_native_channels(&scope);
    }
    // Uninstall is terminal: the capabilities go too, and a leftover renderer closure stops writing
    // as this plugin. A disable keeps them for its unload handler.
    if all {
        with_state(|state| {
            state.plugin_manager.revoke_all_capabilities();
            state.plugin_manager.forget_all_plugins();
        });
    } else {
        with_state(|state| {
            state.plugin_manager.revoke_capabilities(target);
            state.plugin_manager.forget_plugin(target);
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/uninstall.rs"]
mod tests;
