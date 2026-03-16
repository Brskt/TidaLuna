use std::collections::HashMap;

use rquickjs::loader::{Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::{Ctx, Error, Module, Result as JsResult};

use crate::js_runtime::transpile;

// ---------------------------------------------------------------------------
// LunaResolver — resolves module specifiers to canonical names
// ---------------------------------------------------------------------------

/// Resolves known module names (@luna/*, @inrixia/helpers, oby, react, etc.)
/// and plugin URLs to canonical module IDs.
pub struct LunaResolver;

impl Resolver for LunaResolver {
    fn resolve(&mut self, _ctx: &Ctx<'_>, base: &str, name: &str) -> JsResult<String> {
        // Known built-in modules — return as-is
        if is_builtin(name) {
            return Ok(name.to_string());
        }

        // Relative imports (./foo, ../bar) — resolve against base
        if name.starts_with("./") || name.starts_with("../") {
            let resolved = resolve_relative(base, name);
            return Ok(resolved);
        }

        // Plugin URLs (http://, https://) — return as-is
        if name.starts_with("http://") || name.starts_with("https://") {
            return Ok(name.to_string());
        }

        // Unknown — try as-is, let loader decide
        Ok(name.to_string())
    }
}

fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "@luna/core"
            | "@luna/lib"
            | "@luna/lib.native"
            | "@luna/ui"
            | "@luna/dev"
            | "@luna/linux"
            | "@inrixia/helpers"
            | "oby"
            | "react"
            | "react-dom/client"
            | "react/jsx-runtime"
            | "idb-keyval"
    )
}

/// Resolve a relative import against a base module path.
fn resolve_relative(base: &str, relative: &str) -> String {
    // Split base into directory parts
    let mut parts: Vec<&str> = if base.contains('/') {
        let idx = base.rfind('/').unwrap();
        base[..idx].split('/').collect()
    } else {
        vec![]
    };

    for segment in relative.split('/') {
        match segment {
            "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }

    parts.join("/")
}

// ---------------------------------------------------------------------------
// LunaLoader — loads module source code
// ---------------------------------------------------------------------------

/// Loads module source code from built-in registry or plugin store.
pub struct LunaLoader {
    /// Built-in module sources: name → JS source code
    modules: HashMap<String, String>,
}

impl LunaLoader {
    pub fn new() -> Self {
        Self {
            modules: HashMap::new(),
        }
    }

    /// Register a built-in module with its source code.
    /// If the source is TypeScript, it will be transpiled to JS.
    pub fn with_module(mut self, name: &str, source: &str) -> Self {
        let js = if name.ends_with(".ts") || source.contains(": ") && !source.contains("\":") {
            // Heuristic: try transpiling as TS, fallback to raw if it fails
            transpile::transpile_ts(source, &format!("{name}.ts")).unwrap_or_else(|_| source.to_string())
        } else {
            source.to_string()
        };
        self.modules.insert(name.to_string(), js);
        self
    }

    /// Register a built-in module with raw JS (no transpilation).
    pub fn with_js_module(mut self, name: &str, source: &str) -> Self {
        self.modules.insert(name.to_string(), source.to_string());
        self
    }

    /// Register a module source at runtime (e.g., from PluginStore).
    pub fn add_module(&mut self, name: &str, source: String) {
        self.modules.insert(name.to_string(), source);
    }

    /// Check if a module is registered.
    pub fn has_module(&self, name: &str) -> bool {
        self.modules.contains_key(name)
    }
}

impl Loader for LunaLoader {
    fn load<'js>(&mut self, ctx: &Ctx<'js>, name: &str) -> JsResult<Module<'js, Declared>> {
        let source = self
            .modules
            .get(name)
            .ok_or_else(|| Error::new_loading(name))?;

        Module::declare(ctx.clone(), name, source.as_str())
            .map_err(|e| {
                eprintln!("[LunaLoader] Failed to declare module '{name}': {e}");
                e
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::js_runtime;
    use rquickjs::{Module, Runtime};

    fn setup_runtime_with_loader(loader: LunaLoader) -> (Runtime, rquickjs::Context) {
        let (rt, ctx) = js_runtime::init().expect("init");
        rt.set_loader(LunaResolver, loader);
        (rt, ctx)
    }

    #[test]
    fn test_resolve_builtin() {
        let mut resolver = LunaResolver;
        let rt = Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            let result = resolver.resolve(&ctx, "main", "@luna/core").unwrap();
            assert_eq!(result, "@luna/core");
        });
    }

    #[test]
    fn test_resolve_relative() {
        assert_eq!(resolve_relative("@luna/lib/classes/index", "./Album"), "@luna/lib/classes/Album");
        assert_eq!(resolve_relative("@luna/lib/classes/Album", "../helpers/getCredentials"), "@luna/lib/helpers/getCredentials");
        assert_eq!(resolve_relative("main", "./utils"), "utils");
    }

    #[test]
    fn test_load_builtin_module() {
        let loader = LunaLoader::new()
            .with_js_module("@luna/core", "export const version = '1.0';");

        let (_rt, ctx) = setup_runtime_with_loader(loader);

        ctx.with(|ctx| {
            let module = Module::evaluate(
                ctx.clone(),
                "main",
                "import { version } from '@luna/core'; globalThis.__version = version;",
            )
            .unwrap()
            .finish::<()>()
            .unwrap();

            let version: String = ctx.globals().get("__version").unwrap();
            assert_eq!(version, "1.0");
        });
    }

    #[test]
    fn test_load_ts_module() {
        let loader = LunaLoader::new()
            .with_module("mylib", "export const x: number = 42;");

        let (_rt, ctx) = setup_runtime_with_loader(loader);

        ctx.with(|ctx| {
            Module::evaluate(
                ctx.clone(),
                "main",
                "import { x } from 'mylib'; globalThis.__x = x;",
            )
            .unwrap()
            .finish::<()>()
            .unwrap();

            let x: i32 = ctx.globals().get("__x").unwrap();
            assert_eq!(x, 42);
        });
    }

    #[test]
    fn test_load_plugin_module() {
        let mut loader = LunaLoader::new();
        loader.add_module(
            "https://example.com/my-plugin",
            "export function hello() { return 'world'; }".to_string(),
        );

        let (_rt, ctx) = setup_runtime_with_loader(loader);

        ctx.with(|ctx| {
            Module::evaluate(
                ctx.clone(),
                "main",
                "import { hello } from 'https://example.com/my-plugin'; globalThis.__hello = hello();",
            )
            .unwrap()
            .finish::<()>()
            .unwrap();

            let result: String = ctx.globals().get("__hello").unwrap();
            assert_eq!(result, "world");
        });
    }
}
