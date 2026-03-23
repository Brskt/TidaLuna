use super::store::PluginStore;
use super::transpile;
use super::wrapper;

/// Manages plugin lifecycle: transpile, wrap, prepare for CEF injection.
///
/// The PluginManager does NOT inject code into CEF directly — it returns
/// JS strings that the caller (main.rs) passes to `eval_js()`.
///
/// Flow:
///   1. Plugin .mjs loaded from DB (PluginStore)
///   2. Transpiled TS→JS if needed (OXC)
///   3. Wrapped in security closure (wrapper.rs)
///   4. Returned as a JS string for injection into CEF
#[derive(Default)]
pub struct PluginManager {
    /// Track which plugins are currently loaded (for unload).
    loaded: std::collections::HashSet<String>,
}

impl PluginManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Transpile and wrap plugin code for CEF injection (pure, no state mutation).
    pub fn transpile_and_wrap(plugin_id: &str, code: &str) -> anyhow::Result<String> {
        let js = transpile::transpile_ts(code, &format!("{plugin_id}.mts"))?;
        let js = transpile::strip_esm_syntax(&js);
        Ok(wrapper::wrap_plugin_code(plugin_id, &js))
    }

    /// Mark a plugin as loaded.
    pub fn mark_loaded(&mut self, plugin_id: &str) {
        self.loaded.insert(plugin_id.to_string());
    }

    /// Collect enabled plugins' code from the store (read-only, no transpilation).
    pub fn collect_enabled_code(store: &PluginStore) -> Vec<(String, String, String)> {
        let plugins = store.list();
        let mut result = Vec::new();
        for info in &plugins {
            if !info.enabled {
                continue;
            }
            let Some(code) = store.get_code(&info.url) else {
                crate::vprintln!("[PLUGIN] No code found for '{}'", info.url);
                continue;
            };
            result.push((info.url.clone(), info.name.clone(), code));
        }
        result
    }

    /// Generate JS to unload a plugin (calls the plugin's cleanup callbacks).
    ///
    /// Returns JS code to eval in CEF, or None if the plugin wasn't loaded.
    pub fn unload_plugin(&mut self, plugin_id: &str) -> Option<String> {
        if !self.loaded.remove(plugin_id) {
            return None;
        }

        let escaped = wrapper::escape_js_for_sq(plugin_id);
        Some(format!(
            "if(window.__pluginUnloads&&window.__pluginUnloads['{escaped}']){{window.__pluginUnloads['{escaped}']()}}"
        ))
    }

    /// Check if a plugin is currently loaded.
    #[allow(dead_code)]
    pub fn is_loaded(&self, plugin_id: &str) -> bool {
        self.loaded.contains(plugin_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transpile_and_wrap_wraps_code() {
        let result =
            PluginManager::transpile_and_wrap("test-plugin", "console.log('hello');").unwrap();

        assert!(result.starts_with("(function("));
        assert!(result.contains("console.log("));
        assert!(result.contains("hello"));
        assert!(result.contains("'use strict'"));
    }

    #[test]
    fn test_transpile_and_wrap_transpiles_ts() {
        let ts_code = "const x: number = 42; console.log(x);";
        let result = PluginManager::transpile_and_wrap("ts-plugin", ts_code).unwrap();

        assert!(!result.contains(": number"));
        assert!(result.contains("42"));
    }

    #[test]
    fn test_mark_loaded_tracks() {
        let mut mgr = PluginManager::new();
        assert!(!mgr.is_loaded("my-plugin"));

        mgr.mark_loaded("my-plugin");
        assert!(mgr.is_loaded("my-plugin"));
    }

    #[test]
    fn test_unload_plugin_generates_cleanup_js() {
        let mut mgr = PluginManager::new();
        mgr.mark_loaded("my-plugin");

        let js = mgr.unload_plugin("my-plugin");
        assert!(js.is_some());

        let js = js.unwrap();
        assert!(js.contains("__pluginUnloads"));
        assert!(js.contains("my-plugin"));
    }

    #[test]
    fn test_unload_unknown_plugin_returns_none() {
        let mut mgr = PluginManager::new();
        assert!(mgr.unload_plugin("unknown").is_none());
    }

    #[test]
    fn test_unload_removes_from_loaded() {
        let mut mgr = PluginManager::new();
        mgr.mark_loaded("my-plugin");
        mgr.unload_plugin("my-plugin");
        assert!(!mgr.is_loaded("my-plugin"));
    }
}
