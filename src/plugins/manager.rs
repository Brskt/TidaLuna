use super::transpile;
use super::wrapper;

/// Runtime state of a plugin in the CEF renderer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PluginState {
    /// Code dispatched to renderer, awaiting `plugin_ready` ack.
    /// Fields: (load_id, nonce)
    Loading(u64, u64),
    /// Ack received - plugin fully initialized.
    /// Fields: (load_id, nonce)
    Ready(u64, u64),
}

/// Random `u64` nonce for the plugin-load ack handshake, or `None` if the
/// system RNG is unavailable. Generated off the AppState lock by the caller.
pub(crate) fn random_nonce() -> Option<u64> {
    random_nonce_with(|buf| match getrandom::fill(buf) {
        Ok(()) => true,
        Err(e) => {
            crate::vprintln!("[plugins] nonce entropy failure: {e}");
            false
        }
    })
}

/// `fill` returns true on success; entropy source is injected for testing.
fn random_nonce_with(fill: impl FnOnce(&mut [u8]) -> bool) -> Option<u64> {
    let mut buf = [0u8; 8];
    if !fill(&mut buf) {
        return None;
    }
    Some(u64::from_le_bytes(buf))
}

/// Opaque capability for one plugin load, or `None` if the system RNG is unavailable. Generated off
/// the AppState lock by the caller, like the ack nonce.
///
/// Injected into the plugin's wrapper closure and never assigned to a global: a plugin can present
/// the one it was handed or nothing, but not another plugin's.
pub(crate) fn random_capability() -> Option<String> {
    random_capability_with(|buf| match getrandom::fill(buf) {
        Ok(()) => true,
        Err(e) => {
            // Gated, like `random_nonce` beside it: every caller answers "RNG unavailable" to the
            // renderer. The log is not the only channel.
            crate::vprintln!("[plugins] capability entropy failure: {e}");
            false
        }
    })
}

/// `fill` returns true on success; entropy source is injected for testing.
fn random_capability_with(fill: impl FnOnce(&mut [u8]) -> bool) -> Option<String> {
    let mut buf = [0u8; 32];
    if !fill(&mut buf) {
        return None;
    }
    Some(base16ct::lower::encode_string(&buf))
}

/// Live capabilities kept per plugin. A reload leaves the previous one behind for its cleanup; this
/// covers chained reloads while bounding a plugin that reloads in a loop. Per plugin rather than
/// global, keeping such a loop from evicting another plugin's capability and 403ing its writes.
const MAX_CAPABILITIES_PER_PLUGIN: usize = 4;

struct IssuedCapability {
    plugin_id: String,
    /// Issue order: the bound drops this plugin's oldest.
    seq: u64,
    /// The session this capability was issued under.
    ///
    /// Attribution deliberately ignores it: an `onUnload` continuing past a logout still has to
    /// write its settings, which is why the capability outlives the unload at all. What must
    /// not outlive the session is the LIVE CREDENTIAL, so the only reader that compares this is
    /// the one that hands out that token.
    session_epoch: u64,
}

/// Manages plugin lifecycle: transpile, wrap, prepare for CEF injection.
///
/// The PluginManager does NOT inject code into CEF directly - it returns
/// JS strings that the caller passes to `eval_js()`.
///
/// Flow:
///   1. Plugin .mjs loaded from DB (PluginStore)
///   2. Transpiled TS->JS if needed (OXC)
///   3. Wrapped in security closure (wrapper.rs)
///   4. Returned as a JS string for injection into CEF
#[derive(Default)]
pub struct PluginManager {
    states: std::collections::HashMap<String, PluginState>,
    /// Plugin URL -> the manifest name that url declared for itself, for checking what a
    /// `registerNative` caller claims to be.
    ///
    /// Keyed by the url because that is the half that is unique: a manifest name is whatever a
    /// plugin's `package.json` says, and keyed by the name the second plugin to load took the
    /// entry, leaving the first with none and refusing a legitimate registration.
    url_to_name: std::collections::HashMap<String, String>,
    /// Capability to the load it was issued for. Dropped at uninstall, or past the per-plugin bound.
    ///
    /// A reload deliberately does NOT supersede: `disable()` only dispatches the cleanup JS; an
    /// async `onUnload` continuing after an await still needs to be attributable. Costs nothing, since
    /// every capability for one plugin resolves to the same url (the bound is hygiene, not a control).
    capabilities: std::collections::HashMap<String, IssuedCapability>,
    next_load_id: u64,
}

impl PluginManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Transpile and wrap plugin code for CEF injection.
    /// The `load_id` is injected into the wrapper for ack correlation.
    pub fn transpile_and_wrap(
        plugin_id: &str,
        code: &str,
        load_id: u64,
        nonce: u64,
        capability: &str,
    ) -> anyhow::Result<String> {
        let js = transpile::transpile_ts(code, &format!("{plugin_id}.mts"))?;
        Ok(wrapper::wrap_plugin_code(
            plugin_id, &js, load_id, nonce, capability,
        ))
    }

    /// Mark a plugin as Loading (code dispatched, awaiting ack).
    /// `name` is the manifest name (e.g. "DiscordRPC"), recorded against this url so
    /// `registerNative` can check what a caller claims to be. The `nonce` and the
    /// `capability` are generated by the caller off the AppState lock and injected
    /// into the wrapper alongside the returned `load_id`.
    ///
    /// The capability is registered here rather than through a method of its own: it cannot
    /// exist for a load that does not.
    pub fn mark_loading(
        &mut self,
        plugin_id: &str,
        name: &str,
        nonce: u64,
        capability: &str,
        session_epoch: u64,
    ) -> u64 {
        let load_id = self.next_load_id;
        self.next_load_id += 1;
        self.states
            .insert(plugin_id.to_string(), PluginState::Loading(load_id, nonce));
        self.url_to_name
            .insert(plugin_id.to_string(), name.to_string());
        self.capabilities.insert(
            capability.to_string(),
            IssuedCapability {
                plugin_id: plugin_id.to_string(),
                seq: load_id,
                session_epoch,
            },
        );
        self.bound_capabilities_for(plugin_id);
        load_id
    }

    /// Drop a plugin's capabilities, on UNINSTALL only (empty id means all, as `clear_pending_trust`
    /// and `clear_native_channels` also read it). Not on disable, where `onUnload` still needs to be
    /// attributable. Races that handler since `uninstall()` awaits `disable()` first (accepted,
    /// because uninstall deletes the plugin's storage rows anyway).
    pub fn revoke_capabilities(&mut self, plugin_id: &str) {
        if plugin_id.is_empty() {
            self.capabilities.clear();
            return;
        }
        self.capabilities
            .retain(|_, issued| issued.plugin_id != plugin_id);
    }

    /// Drop this plugin's oldest capabilities past the bound.
    fn bound_capabilities_for(&mut self, plugin_id: &str) {
        let mut seqs: Vec<u64> = self
            .capabilities
            .values()
            .filter(|issued| issued.plugin_id == plugin_id)
            .map(|issued| issued.seq)
            .collect();
        if seqs.len() <= MAX_CAPABILITIES_PER_PLUGIN {
            return;
        }
        seqs.sort_unstable();
        let cutoff = seqs[seqs.len() - MAX_CAPABILITIES_PER_PLUGIN];
        self.capabilities
            .retain(|_, issued| issued.plugin_id != plugin_id || issued.seq >= cutoff);
    }

    /// The plugin a capability was issued to, or `None` if it was never issued or has been
    /// revoked. An empty capability never resolves: a missing envelope field cannot match.
    pub fn plugin_for_capability(&self, capability: &str) -> Option<String> {
        if capability.is_empty() {
            return None;
        }
        self.capabilities
            .get(capability)
            .map(|issued| issued.plugin_id.clone())
    }

    /// The session a capability was issued under, for the one caller that hands out a live
    /// credential and must refuse to hand it to a session the user has left.
    ///
    /// Separate from [`PluginManager::plugin_for_capability`] on purpose, attribution and
    /// authorization being different questions: a plugin unloading after a logout is still the
    /// plugin that wrote its settings, and answering "who is this" with `None` would break the
    /// `onUnload` contract that keeps the capability alive.
    pub fn capability_epoch(&self, capability: &str) -> Option<u64> {
        if capability.is_empty() {
            return None;
        }
        self.capabilities
            .get(capability)
            .map(|issued| issued.session_epoch)
    }

    /// The manifest name this url declared for itself, which is the only thing a caller's claim may
    /// be checked against: a name that arrives as an argument says what the caller wants to be.
    pub fn name_for_url(&self, plugin_id: &str) -> Option<&str> {
        self.url_to_name.get(plugin_id).map(|s| s.as_str())
    }

    /// Transition Loading -> Ready if load_id AND nonce match. Returns true if accepted.
    pub fn mark_ready(&mut self, plugin_id: &str, load_id: u64, nonce: u64) -> bool {
        match self.states.get(plugin_id) {
            Some(PluginState::Loading(cid, cn)) if *cid == load_id && *cn == nonce => {
                self.states
                    .insert(plugin_id.to_string(), PluginState::Ready(load_id, nonce));
                true
            }
            _ => false,
        }
    }

    /// Generate the JS cleanup code for a plugin (static, no state mutation).
    pub fn generate_unload_js(plugin_id: &str) -> String {
        let escaped = wrapper::escape_js(plugin_id);
        format!(
            "if(window.__pluginUnloads&&window.__pluginUnloads['{escaped}']){{window.__pluginUnloads['{escaped}']()}}"
        )
    }

    /// End the load `load_id`, whether it reached Ready or not. Returns true if this call is
    /// what removed it.
    ///
    /// Naming the load is the whole point. Removal used to be unconditional, and nothing
    /// serialises `plugin.enable` against `plugin.disable`: a disable that lost the race erased
    /// the `Loading` entry a newer enable had just written, while that enable injected its code
    /// anyway. The plugin then ran in the renderer with no map knowing it, so no sweep unloads
    /// it and the ten-second watchdog looks it up, finds nothing, and concludes there is
    /// nothing to rescue.
    ///
    /// The capability outlives this on purpose: `eval_js` only dispatches the cleanup, so the
    /// plugin's `onUnload` runs after this returns and its settings write still has to be
    /// attributable. Superseded at the next `mark_loading` instead.
    pub fn retire_load(&mut self, plugin_id: &str, load_id: u64) -> bool {
        match self.states.get(plugin_id) {
            Some(PluginState::Loading(id, _) | PluginState::Ready(id, _)) if *id == load_id => {
                self.forget_plugin(plugin_id);
                true
            }
            _ => false,
        }
    }

    /// Abandon `load_id` only while it is still LOADING. Returns true if this call removed it.
    ///
    /// For the watchdogs, which fire on a load that never acked. Ready is deliberately not
    /// matched: an ack landing between the check and the removal means the load succeeded at the
    /// last moment, and tearing it down would destroy a plugin that works. Test and removal
    /// happen under one borrow, two `with_state` calls being the window that ack arrives through.
    pub fn abandon_if_still_loading(&mut self, plugin_id: &str, load_id: u64) -> bool {
        match self.states.get(plugin_id) {
            Some(PluginState::Loading(id, _)) if *id == load_id => {
                self.forget_plugin(plugin_id);
                true
            }
            _ => false,
        }
    }

    /// Drop every trace of a plugin, whichever load is current. For UNINSTALL only, where the
    /// DB row is gone: a conditional removal that lost a race would leave an entry standing for
    /// a plugin no path can ever reach again, which is worse than the race it avoids.
    pub fn forget_plugin(&mut self, plugin_id: &str) {
        self.states.remove(plugin_id);
        self.url_to_name.remove(plugin_id);
    }

    /// Every loaded plugin with the load that owns it, taken together.
    ///
    /// One snapshot, not a list of urls each of whose load is looked up afterwards: the
    /// second read happens under a different lock acquisition, which is where a concurrent
    /// enable slips in and makes the pair describe two different loads.
    pub fn loaded_loads(&self) -> Vec<(String, u64)> {
        self.states
            .iter()
            .map(|(url, state)| match state {
                PluginState::Loading(id, _) | PluginState::Ready(id, _) => (url.clone(), *id),
            })
            .collect()
    }

    /// Retire every load this manager has live, and name them.
    ///
    /// The bulk form is the point: naming the loads and dropping them in one call lets a session
    /// end without handing the lock back between the two. A plugin registering in that gap would
    /// be named by neither, and the injection it is about to post would answer to nothing.
    pub fn retire_all_loaded(&mut self) -> Vec<(String, u64)> {
        let loaded = self.loaded_loads();
        for (url, load_id) in &loaded {
            self.retire_load(url, *load_id);
        }
        loaded
    }

    /// Get the current load_id for a plugin, if any.
    pub fn current_load_id(&self, plugin_id: &str) -> Option<u64> {
        self.states.get(plugin_id).map(|s| match s {
            PluginState::Loading(id, _) | PluginState::Ready(id, _) => *id,
        })
    }
}

#[cfg(test)]
#[path = "../../tests/unit/plugins/manager.rs"]
mod tests;
