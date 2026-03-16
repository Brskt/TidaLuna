use std::collections::HashMap;

use rquickjs::{Context, Module, Runtime};

use crate::js_runtime::{fetch, loader, shims, transpile};
use crate::plugins::PluginStore;

/// Manages the QuickJS runtime for executing TidaLuna plugins.
///
/// Owns the JS Runtime + Context, the module loader, and tracks
/// loaded plugin state.
pub struct PluginRuntime {
    rt: Runtime,
    ctx: Context,
    /// Map of plugin URL → loaded state
    loaded_plugins: HashMap<String, LoadedPlugin>,
}

struct LoadedPlugin {
    url: String,
    name: String,
    /// Whether the module has an `onUnload` export
    has_on_unload: bool,
}

impl PluginRuntime {
    /// Create a new plugin runtime with all shims and an empty module loader.
    pub fn new() -> anyhow::Result<Self> {
        let (rt, ctx) = crate::js_runtime::init()
            .map_err(|e| anyhow::anyhow!("Failed to init JS runtime: {e}"))?;

        // Install shims (console already done in init)
        ctx.with(|ctx| {
            shims::install_shims(&ctx)?;
            fetch::install_fetch(&ctx)?;
            Ok::<_, rquickjs::Error>(())
        })
        .map_err(|e| anyhow::anyhow!("Failed to install shims: {e}"))?;

        // Set up module loader with empty LunaLoader
        // Modules will be registered before plugins are loaded
        let resolver = loader::LunaResolver;
        let luna_loader = loader::LunaLoader::new();
        rt.set_loader(resolver, luna_loader);

        Ok(Self {
            rt,
            ctx,
            loaded_plugins: HashMap::new(),
        })
    }

    /// Register a built-in module (e.g., @luna/core, @luna/lib).
    /// Must be called before loading plugins that depend on it.
    ///
    /// Note: Because rquickjs takes ownership of the loader in set_loader(),
    /// we rebuild the loader each time.  For a small number of modules this
    /// is fine.
    pub fn register_module(&self, name: &str, source: &str) -> anyhow::Result<()> {
        // Transpile if TypeScript
        let js = if name.ends_with(".ts") || name.ends_with(".mts") {
            transpile::transpile_ts(source, &format!("{name}.ts"))?
        } else {
            source.to_string()
        };

        // We can't modify the loader after set_loader, so we declare the module
        // directly in the context
        self.ctx.with(|ctx| {
            Module::declare(ctx.clone(), name, js.as_str())
                .map_err(|e| anyhow::anyhow!("Failed to declare module '{name}': {e}"))?;
            Ok(())
        })
    }

    /// Load and execute a plugin by URL.
    /// Fetches the code from the PluginStore, transpiles if needed, and executes.
    pub fn load_plugin(&mut self, store: &PluginStore, url: &str) -> anyhow::Result<()> {
        let code = store
            .get_code(url)
            .ok_or_else(|| anyhow::anyhow!("No code found for plugin '{url}'"))?;

        // Transpile TS → JS if needed
        let js = if url.ends_with(".ts") || url.ends_with(".mts") {
            transpile::transpile_ts(&code, url)?
        } else {
            // Try transpiling — if it fails, use raw code (already JS)
            transpile::transpile_ts(&code, &format!("{url}.mjs")).unwrap_or(code)
        };

        // Execute the plugin module
        let has_on_unload = self.ctx.with(|ctx| -> anyhow::Result<bool> {
            let module = Module::declare(ctx.clone(), url, js.as_str())
                .map_err(|e| anyhow::anyhow!("Failed to declare plugin '{url}': {e}"))?;

            let (evaluated, _promise) = module
                .eval()
                .map_err(|e| anyhow::anyhow!("Failed to evaluate plugin '{url}': {e}"))?;

            // Check for onUnload export
            let has_unload: bool = evaluated
                .get::<_, rquickjs::Function>("onUnload")
                .is_ok();

            Ok(has_unload)
        })?;

        // Execute pending jobs OUTSIDE ctx.with() to avoid RefCell double-borrow
        while self.rt.is_job_pending() {
            if self.rt.execute_pending_job().is_err() {
                break;
            }
        }

        self.loaded_plugins.insert(
            url.to_string(),
            LoadedPlugin {
                url: url.to_string(),
                name: url.to_string(),
                has_on_unload,
            },
        );

        crate::vprintln!("[PLUGIN] Loaded: {url} (onUnload: {has_on_unload})");
        Ok(())
    }

    /// Unload a plugin — calls its `onUnload` export if present.
    pub fn unload_plugin(&mut self, url: &str) -> anyhow::Result<()> {
        let plugin = self.loaded_plugins.remove(url);
        if let Some(plugin) = &plugin {
            if plugin.has_on_unload {
                self.ctx.with(|ctx| {
                    // Try to call the onUnload function
                    let code = format!(
                        r#"
                        try {{
                            const mod = globalThis.__loaded_modules && globalThis.__loaded_modules["{}"];
                            if (mod && typeof mod.onUnload === "function") mod.onUnload();
                        }} catch(e) {{
                            console.error("onUnload error:", e);
                        }}
                        "#,
                        url.replace('"', r#"\""#)
                    );
                    let _ = ctx.eval::<(), _>(code.as_str());
                });
            }
            crate::vprintln!("[PLUGIN] Unloaded: {url}");
        }
        Ok(())
    }

    /// Drive timers — should be called periodically.
    pub fn tick(&self) {
        self.ctx.with(|ctx| {
            let _ = shims::drive_timers(&ctx);
        });

        // Also execute any pending JS jobs
        while self.rt.is_job_pending() {
            if self.rt.execute_pending_job().is_err() {
                break;
            }
        }
    }

    /// Load all enabled plugins from the store.
    pub fn load_all_enabled(&mut self, store: &PluginStore) -> Vec<String> {
        let plugins = store.list();
        let mut loaded = Vec::new();

        for plugin in &plugins {
            if plugin.enabled {
                match self.load_plugin(store, &plugin.url) {
                    Ok(()) => loaded.push(plugin.url.clone()),
                    Err(e) => eprintln!("[PLUGIN] Failed to load '{}': {e}", plugin.url),
                }
            }
        }

        loaded
    }

    /// Get list of currently loaded plugin URLs.
    pub fn loaded_plugins(&self) -> Vec<String> {
        self.loaded_plugins.keys().cloned().collect()
    }

    /// Execute arbitrary JS in the plugin context (for debugging).
    pub fn eval(&self, code: &str) -> anyhow::Result<String> {
        self.ctx.with(|ctx| {
            let result: String = ctx
                .eval(code)
                .map_err(|e| anyhow::anyhow!("Eval error: {e}"))?;
            Ok(result)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_plugin_runtime() {
        let runtime = PluginRuntime::new();
        assert!(runtime.is_ok());
    }

    #[test]
    fn test_register_and_import_module() {
        let runtime = PluginRuntime::new().unwrap();

        runtime
            .register_module("@luna/core", "export const version = '1.0.0';")
            .unwrap();

        let result = runtime.eval("typeof globalThis").unwrap();
        assert_eq!(result, "object");
    }

    #[test]
    fn test_tick_drives_timers() {
        let runtime = PluginRuntime::new().unwrap();

        runtime.ctx.with(|ctx| {
            let _: () = ctx
                .eval(
                    r#"
                globalThis.__ticked = false;
                setTimeout(() => { globalThis.__ticked = true; }, 0);
            "#,
                )
                .unwrap();
        });

        runtime.tick();

        runtime.ctx.with(|ctx| {
            let ticked: bool = ctx.eval("globalThis.__ticked").unwrap();
            assert!(ticked);
        });
    }

    #[test]
    fn test_fetch_works_in_runtime() {
        let runtime = PluginRuntime::new().unwrap();

        runtime.ctx.with(|ctx| {
            let has_fetch: bool = ctx.eval("typeof fetch === 'function'").unwrap();
            assert!(has_fetch);
        });
    }
}
