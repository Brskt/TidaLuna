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
pub struct PluginManager {
    /// Track which plugins are currently loaded (for unload).
    loaded: std::collections::HashSet<String>,
}

impl PluginManager {
    pub fn new() -> Self {
        Self {
            loaded: std::collections::HashSet::new(),
        }
    }

    /// Prepare a plugin for CEF injection.
    ///
    /// Returns the wrapped JS code ready to be passed to `eval_js()`,
    /// or an error if transpilation fails.
    pub fn prepare_plugin(&mut self, plugin_id: &str, code: &str) -> anyhow::Result<String> {
        // Transpile if it looks like TypeScript
        let js = if needs_transpile(code) {
            transpile::transpile_ts(code, &format!("{plugin_id}.mts"))?
        } else {
            code.to_string()
        };

        // Strip ES module syntax (export/import) so code runs in IIFE context.
        // Quartz-bundled plugins have exports at the end and imports already resolved.
        let js = transpile::strip_esm_syntax(&js);

        // Wrap in security closure
        let wrapped = wrapper::wrap_plugin_code(plugin_id, &js);

        self.loaded.insert(plugin_id.to_string());
        Ok(wrapped)
    }

    /// Prepare all enabled plugins from the store.
    ///
    /// Returns a Vec of (plugin_id, wrapped_js) for each successfully prepared plugin.
    /// Logs errors for plugins that fail to prepare.
    pub fn prepare_all_enabled(&mut self, store: &PluginStore) -> Vec<(String, String)> {
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

            match self.prepare_plugin(&info.url, &code) {
                Ok(wrapped) => {
                    crate::vprintln!(
                        "[PLUGIN] Prepared '{}' ({} bytes)",
                        info.name,
                        wrapped.len()
                    );
                    result.push((info.url.clone(), wrapped));
                }
                Err(e) => {
                    crate::vprintln!("[PLUGIN] Failed to prepare '{}': {e}", info.name);
                }
            }
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

/// Heuristic: does the source code look like TypeScript?
fn needs_transpile(code: &str) -> bool {
    // Check for common TypeScript indicators
    code.contains(": string")
        || code.contains(": number")
        || code.contains(": boolean")
        || code.contains(": void")
        || code.contains("interface ")
        || code.contains("<T>")
        || code.contains("as ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prepare_plugin_wraps_code() {
        let mut mgr = PluginManager::new();
        let result = mgr
            .prepare_plugin("test-plugin", "console.log('hello');")
            .unwrap();

        // Should be wrapped in IIFE
        assert!(result.starts_with("(function("));
        assert!(result.contains("console.log('hello');"));
        assert!(result.contains("'use strict'"));
    }

    #[test]
    fn test_prepare_plugin_transpiles_ts() {
        let mut mgr = PluginManager::new();
        let ts_code = "const x: number = 42; console.log(x);";
        let result = mgr.prepare_plugin("ts-plugin", ts_code).unwrap();

        // Type annotation should be stripped
        assert!(!result.contains(": number"));
        assert!(result.contains("42"));
    }

    #[test]
    fn test_prepare_plugin_tracks_loaded() {
        let mut mgr = PluginManager::new();
        assert!(!mgr.is_loaded("my-plugin"));

        mgr.prepare_plugin("my-plugin", "").unwrap();
        assert!(mgr.is_loaded("my-plugin"));
    }

    #[test]
    fn test_unload_plugin_generates_cleanup_js() {
        let mut mgr = PluginManager::new();
        mgr.prepare_plugin("my-plugin", "").unwrap();

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
        mgr.prepare_plugin("my-plugin", "").unwrap();
        mgr.unload_plugin("my-plugin");
        assert!(!mgr.is_loaded("my-plugin"));
    }

    #[test]
    fn test_needs_transpile() {
        assert!(needs_transpile("const x: number = 1;"));
        assert!(needs_transpile("interface Foo { bar: string }"));
        assert!(!needs_transpile("const x = 1;"));
        assert!(!needs_transpile("console.log('hello');"));
    }
}
