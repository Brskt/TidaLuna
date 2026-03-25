//! Security wrapper for plugin code injected into CEF.
//!
//! Wraps plugin JS in an IIFE that:
//!   - Shadows dangerous globals (localStorage, eval, XMLHttpRequest, etc.)
//!   - Provides a controlled `fetch()` that routes through Rust (token injection, audit)
//!   - Provides a controlled `storage` API (per-plugin SQLite via IPC)
//!   - Tracks unload callbacks for cleanup
//!
//! The real `window.cefQuery` is captured as a private `__cq` parameter —
//! used internally by the controlled APIs but not exposed to plugin code.

/// Escape a string for safe embedding in a JS single-quoted string literal.
pub(super) fn escape_js_for_sq(s: &str) -> String {
    escape_js(s)
}

fn escape_js(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\0', "\\0")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// Wrap plugin code in a security closure for CEF injection.
///
/// `plugin_id` — unique identifier (URL) for the plugin, used for storage namespace & audit.
/// `code` — already transpiled+bundled JS (no ES module syntax, ready for eval).
///
/// Returns a self-executing JS string safe to pass to `frame.execute_java_script()`.
pub fn wrap_plugin_code(plugin_id: &str, code: &str) -> String {
    let escaped_id = escape_js(plugin_id);

    // Template uses string concatenation to avoid format!() escaping hell with JS braces.
    // The IIFE parameters shadow dangerous globals — they receive `undefined` as arguments.
    let mut js = String::with_capacity(code.len() + 4096);

    // --- Two-layer IIFE ---
    // Outer (sloppy mode): shadows `eval` and `Function` as parameters.
    // These names are forbidden in strict mode parameter lists, so the outer
    // IIFE must NOT have 'use strict'.
    // Inner (strict mode): shadows all other dangerous globals as parameters.
    js.push_str("(function(eval, Function) {\n");
    // Inner IIFE is async to support top-level await (plugin .mjs files are ES modules).
    js.push_str("(async function(__pid, __cq, localStorage, sessionStorage, XMLHttpRequest, indexedDB, caches, ServiceWorker, importScripts) {\n");
    js.push_str("'use strict';\n");

    // --- Controlled fetch ---
    // Routes all network requests through Rust via cefQuery.
    // Rust injects the OAuth token and can filter/audit URLs.
    js.push_str(r#"
var fetch = function(input, init) {
    var url = typeof input === 'string' ? input : (input && input.url) || String(input);
    var opts = init || {};
    var method = opts.method || 'GET';
    var headers = {};
    if (opts.headers) {
        if (typeof opts.headers.entries === 'function') {
            for (var pair of opts.headers.entries()) headers[pair[0]] = pair[1];
        } else if (typeof opts.headers === 'object') {
            headers = opts.headers;
        }
    }
    return new Promise(function(resolve, reject) {
        __cq({
            request: JSON.stringify({ channel: 'plugin.fetch', args: [__pid, url, JSON.stringify({ method: method, headers: headers, body: opts.body || null })], id: String(++fetch.__seq) }),
            onSuccess: function(raw) {
                try {
                    var r = JSON.parse(raw);
                    resolve({
                        ok: r.ok, status: r.status, statusText: r.statusText, url: r.url,
                        headers: new Headers(r.headers || {}),
                        text: function() { return Promise.resolve(r.body); },
                        json: function() { return Promise.resolve(JSON.parse(r.body)); },
                        arrayBuffer: function() { return Promise.resolve(new TextEncoder().encode(r.body).buffer); },
                        clone: function() { return Object.assign({}, this); },
                        blob: function() { return Promise.resolve(new Blob([r.body])); }
                    });
                } catch(e) { reject(e); }
            },
            onFailure: function(code, msg) { reject(new Error('[plugin.fetch] ' + msg)); }
        });
    });
};
fetch.__seq = 0;
"#);

    // --- Controlled storage ---
    // Per-plugin key/value storage backed by Rust SQLite via IPC.
    js.push_str(r#"
var __idbKeyval = {
    get: function(key) {
        return new Promise(function(resolve, reject) {
            __cq({
                request: JSON.stringify({ channel: 'plugin.storage.get', args: [__pid, key], id: String(++__idbKeyval.__seq) }),
                onSuccess: function(raw) {
                    try { resolve(raw === '' ? undefined : JSON.parse(raw)); } catch(e) { resolve(raw); }
                },
                onFailure: function(code, msg) { reject(new Error(msg)); }
            });
        });
    },
    set: function(key, value) {
        return new Promise(function(resolve, reject) {
            __cq({
                request: JSON.stringify({ channel: 'plugin.storage.set', args: [__pid, key, JSON.stringify(value)], id: String(++__idbKeyval.__seq) }),
                onSuccess: function() { resolve(); },
                onFailure: function(code, msg) { reject(new Error(msg)); }
            });
        });
    },
    del: function(key) {
        return new Promise(function(resolve, reject) {
            __cq({
                request: JSON.stringify({ channel: 'plugin.storage.del', args: [__pid, key], id: String(++__idbKeyval.__seq) }),
                onSuccess: function() { resolve(); },
                onFailure: function(code, msg) { reject(new Error(msg)); }
            });
        });
    },
    keys: function() {
        return new Promise(function(resolve, reject) {
            __cq({
                request: JSON.stringify({ channel: 'plugin.storage.keys', args: [__pid], id: String(++__idbKeyval.__seq) }),
                onSuccess: function(raw) {
                    try { resolve(JSON.parse(raw)); } catch(e) { resolve([]); }
                },
                onFailure: function(code, msg) { reject(new Error(msg)); }
            });
        });
    },
    __seq: 0
};
"#);

    // --- Plugin unload tracking ---
    // Plugins register cleanup callbacks via onUnload().
    // Rust calls window.__pluginUnloads[pluginId]() to trigger cleanup.
    js.push_str(
        r#"
var __unloads = [];
var onUnload = function(fn) { if (typeof fn === 'function') __unloads.push(fn); };
if (!window.__pluginUnloads) window.__pluginUnloads = {};
window.__pluginUnloads[__pid] = function() {
    for (var i = __unloads.length - 1; i >= 0; i--) {
        try { __unloads[i](); } catch(e) { console.error('[plugin:unload]', __pid, e); }
    }
    __unloads = [];
    delete window.__pluginUnloads[__pid];
};
"#,
    );

    // --- Plugin code ---
    js.push_str("\n// --- Plugin: ");
    js.push_str(&escaped_id);
    js.push_str(" ---\n");
    js.push_str(code);
    js.push('\n');

    // --- Register exports on window ---
    // strip_esm_syntax converts `export{F as Settings, T as unloads}` to
    // `var __exports = {Settings: F, unloads: T}`. We register this on window
    // so extractSettings() can read it without re-executing the plugin.
    // Also wire the plugin's own unloads Set into our cleanup system.
    js.push_str(
        r#"
if (typeof __exports !== 'undefined') {
    if (!window.__pluginExports) window.__pluginExports = {};
    window.__pluginExports[__pid] = __exports;
    if (__exports.unloads instanceof Set) {
        for (var __fn of __exports.unloads) __unloads.push(__fn);
    }
}
"#,
    );

    // --- Inner IIFE close ---
    // Arguments: pluginId, cefQuery ref, then undefined for all shadowed globals
    js.push_str("})('");
    js.push_str(&escaped_id);
    js.push_str("', window.cefQuery, undefined, undefined, undefined, undefined, undefined, undefined, undefined);\n");

    // --- Outer IIFE close ---
    // Passes undefined for eval and Function
    js.push_str("})(undefined, undefined);\n");

    js
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_produces_iife() {
        let result = wrap_plugin_code("https://example.com/plugin.mjs", "console.log('hello');");
        assert!(result.starts_with("(function("));
        assert!(result.contains("'use strict'"));
        assert!(result.contains("console.log('hello');"));
        assert!(result.trim_end().ends_with(");\n") || result.trim_end().ends_with(");"));
    }

    #[test]
    fn test_wrap_shadows_localstorage() {
        let result = wrap_plugin_code("test", "");
        // localStorage should be a parameter name (shadowed to undefined)
        assert!(result.contains("localStorage"));
        // The IIFE call should pass undefined for it
        assert!(result.contains(", undefined, undefined,"));
    }

    #[test]
    fn test_wrap_contains_controlled_fetch() {
        let result = wrap_plugin_code("test", "");
        assert!(result.contains("var fetch = function("));
        assert!(result.contains("plugin.fetch"));
        assert!(result.contains("__cq("));
    }

    #[test]
    fn test_wrap_contains_storage_api() {
        let result = wrap_plugin_code("test", "");
        assert!(result.contains("__idbKeyval"));
        assert!(result.contains("plugin.storage.get"));
        assert!(result.contains("plugin.storage.set"));
        assert!(result.contains("plugin.storage.del"));
        assert!(result.contains("plugin.storage.keys"));
    }

    #[test]
    fn test_wrap_contains_unload_tracking() {
        let result = wrap_plugin_code("test", "");
        assert!(result.contains("__pluginUnloads"));
        assert!(result.contains("onUnload"));
    }

    #[test]
    fn test_wrap_escapes_plugin_id() {
        let result = wrap_plugin_code("it's a \"test\"", "");
        // Single quotes escaped (embedded in JS single-quoted string)
        assert!(result.contains("it\\'s a"));
        // Double quotes pass through unescaped (safe in single-quoted JS context)
        assert!(result.contains("\"test\""));
    }

    #[test]
    fn test_wrap_shadows_dangerous_apis() {
        let result = wrap_plugin_code("test", "");
        // All these should be parameter names (shadowed)
        // WebSocket intentionally NOT shadowed — plugins like DiscordRPC need it.
        for name in &[
            "eval",
            "Function",
            "localStorage",
            "sessionStorage",
            "XMLHttpRequest",
            "indexedDB",
            "caches",
            "ServiceWorker",
            "importScripts",
        ] {
            assert!(result.contains(name), "Missing shadowed global: {}", name);
        }
    }

    #[test]
    fn test_wrap_cefquery_private() {
        let result = wrap_plugin_code("test", "");
        // cefQuery is captured as __cq (private), not exposed by name
        assert!(result.contains("__cq"));
        assert!(result.contains("window.cefQuery"));
    }
}
