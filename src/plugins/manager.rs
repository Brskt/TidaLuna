use super::transpile;
use super::wrapper;

/// Runtime state of a plugin in the CEF renderer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PluginState {
    /// Code dispatched to renderer, awaiting `plugin_ready` ack.
    /// Fields: (load_id, nonce)
    Loading(u64, u64),
    /// Ack received — plugin fully initialized.
    /// Fields: (load_id, nonce)
    Ready(u64, u64),
}

fn random_nonce() -> u64 {
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).expect("getrandom failed");
    u64::from_le_bytes(buf)
}

/// Manages plugin lifecycle: transpile, wrap, prepare for CEF injection.
///
/// The PluginManager does NOT inject code into CEF directly — it returns
/// JS strings that the caller passes to `eval_js()`.
///
/// Flow:
///   1. Plugin .mjs loaded from DB (PluginStore)
///   2. Transpiled TS→JS if needed (OXC)
///   3. Wrapped in security closure (wrapper.rs)
///   4. Returned as a JS string for injection into CEF
#[derive(Default)]
pub struct PluginManager {
    states: std::collections::HashMap<String, PluginState>,
    /// Reverse mapping: plugin manifest name → plugin URL.
    /// Used to validate `registerNative` callers.
    name_to_url: std::collections::HashMap<String, String>,
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
    ) -> anyhow::Result<String> {
        let js = transpile::transpile_ts(code, &format!("{plugin_id}.mts"))?;
        let js = transpile::strip_esm_syntax(&js);
        Ok(wrapper::wrap_plugin_code(plugin_id, &js, load_id, nonce))
    }

    /// Mark a plugin as Loading (code dispatched, awaiting ack).
    /// `name` is the manifest name (e.g. "DiscordRPC") — stored in the reverse
    /// mapping so `registerNative` can validate the caller.
    /// Returns `(load_id, nonce)` — both injected into the wrapper for ack correlation.
    pub fn mark_loading(&mut self, plugin_id: &str, name: &str) -> (u64, u64) {
        let load_id = self.next_load_id;
        self.next_load_id += 1;
        let nonce = random_nonce();
        self.states
            .insert(plugin_id.to_string(), PluginState::Loading(load_id, nonce));
        self.name_to_url
            .insert(name.to_string(), plugin_id.to_string());
        (load_id, nonce)
    }

    /// Look up the URL for a plugin by manifest name.
    pub fn url_for_name(&self, name: &str) -> Option<&str> {
        self.name_to_url.get(name).map(|s| s.as_str())
    }

    /// Transition Loading → Ready if load_id AND nonce match. Returns true if accepted.
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
        let escaped = wrapper::escape_js_for_sq(plugin_id);
        format!(
            "if(window.__pluginUnloads&&window.__pluginUnloads['{escaped}']){{window.__pluginUnloads['{escaped}']()}}"
        )
    }

    /// Remove a plugin from the state map (call AFTER eval_js dispatch of cleanup).
    pub fn mark_unloaded(&mut self, plugin_id: &str) {
        self.states.remove(plugin_id);
        self.name_to_url.retain(|_, url| url != plugin_id);
    }

    /// All currently loaded plugin URLs.
    pub fn loaded_urls(&self) -> Vec<String> {
        self.states.keys().cloned().collect()
    }

    /// True if no plugins are loaded.
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// True if the plugin is Loading or Ready.
    pub fn is_loaded(&self, plugin_id: &str) -> bool {
        self.states.contains_key(plugin_id)
    }

    /// True only if the plugin has received its ready ack.
    pub fn is_ready(&self, plugin_id: &str) -> bool {
        matches!(self.states.get(plugin_id), Some(PluginState::Ready(..)))
    }

    /// Get the current load_id for a plugin, if any.
    pub fn current_load_id(&self, plugin_id: &str) -> Option<u64> {
        self.states.get(plugin_id).map(|s| match s {
            PluginState::Loading(id, _) | PluginState::Ready(id, _) => *id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transpile_and_wrap_wraps_code() {
        let result =
            PluginManager::transpile_and_wrap("test-plugin", "console.log('hello');", 0, 0)
                .unwrap();

        assert!(result.starts_with("(function("));
        assert!(result.contains("console.log("));
        assert!(result.contains("hello"));
        assert!(result.contains("'use strict'"));
    }

    #[test]
    fn test_transpile_and_wrap_transpiles_ts() {
        let ts_code = "const x: number = 42; console.log(x);";
        let result = PluginManager::transpile_and_wrap("ts-plugin", ts_code, 0, 0).unwrap();

        assert!(!result.contains(": number"));
        assert!(result.contains("42"));
    }

    #[test]
    fn test_mark_loading_returns_load_id_and_nonce() {
        let mut mgr = PluginManager::new();
        let (id1, _) = mgr.mark_loading("a", "a");
        let (id2, _) = mgr.mark_loading("b", "b");
        assert_ne!(id1, id2);
        assert!(mgr.is_loaded("a"));
        assert!(mgr.is_loaded("b"));
    }

    #[test]
    fn test_mark_ready_with_matching_load_id_and_nonce() {
        let mut mgr = PluginManager::new();
        let (load_id, nonce) = mgr.mark_loading("p", "p");
        assert!(!mgr.is_ready("p"));
        assert!(mgr.mark_ready("p", load_id, nonce));
        assert!(mgr.is_ready("p"));
        assert!(mgr.is_loaded("p"));
    }

    #[test]
    fn test_mark_ready_with_stale_load_id_ignored() {
        let mut mgr = PluginManager::new();
        let (old_id, old_nonce) = mgr.mark_loading("p", "p");
        let (_new_id, _new_nonce) = mgr.mark_loading("p", "p"); // reload
        assert!(!mgr.mark_ready("p", old_id, old_nonce)); // stale
        assert!(!mgr.is_ready("p"));
    }

    #[test]
    fn test_mark_ready_with_wrong_nonce_rejected() {
        let mut mgr = PluginManager::new();
        let (load_id, _nonce) = mgr.mark_loading("p", "p");
        assert!(!mgr.mark_ready("p", load_id, 99999)); // wrong nonce
        assert!(!mgr.is_ready("p"));
    }

    #[test]
    fn test_is_loaded_during_loading() {
        let mut mgr = PluginManager::new();
        mgr.mark_loading("p", "p");
        assert!(mgr.is_loaded("p"));
        assert!(!mgr.is_ready("p"));
    }

    #[test]
    fn test_mark_unloaded_clears_state() {
        let mut mgr = PluginManager::new();
        mgr.mark_loading("p", "p");
        mgr.mark_unloaded("p");
        assert!(!mgr.is_loaded("p"));
        assert!(!mgr.is_ready("p"));
    }

    #[test]
    fn test_generate_unload_js_produces_cleanup_code() {
        let js = PluginManager::generate_unload_js("my-plugin");
        assert!(js.contains("__pluginUnloads"));
        assert!(js.contains("my-plugin"));
    }
}
