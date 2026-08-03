//! Security wrapper for plugin code injected into CEF.
//!
//! Wraps plugin JS in an IIFE that:
//!   - Shadows dangerous globals (localStorage, eval, XMLHttpRequest, etc.)
//!   - Shadows sensitive globals (cefQuery, nativeInterface, IPC helpers, token globals)
//!   - Provides a controlled `fetch()` that routes through Rust (token injection, audit)
//!   - Provides a controlled `storage` API (per-plugin SQLite via IPC)
//!   - Tracks unload callbacks for cleanup
//!
//! The real `window.cefQuery` is captured as a private `__cq` parameter -
//! used internally by the controlled APIs but not exposed to plugin code.
//!
//! ## Threat model & limitations
//!
//! This wrapper **reduces accidental token exposure** to third-party plugins.
//! It is **NOT a hard security boundary**.
//!
//! All plugins share the same V8 context as the TIDAL app. A determined malicious
//! plugin can bypass all IIFE-based protections because:
//!   - `document.defaultView` returns the real `Window` object
//!   - `window.window` and `window.self` also return the `Window`
//!   - Prototype chain traversal can reach unshadowed globals
//!   - No amount of JS-level shadowing can fully hide the real `Window`
//!
//! True plugin isolation would require separate V8 isolates, Web Workers,
//! or separate renderer processes.
//!
//! Current protections (defense in depth, weakest to strongest):
//!   1. IIFE parameter shadowing - blocks bare identifiers only (`cefQuery` blocked,
//!      `window.cefQuery` not)
//!   2. XHR/fetch prototype freeze - `Object.defineProperty` with `writable: false,
//!      configurable: false` (silent failure on reassignment in sloppy mode)
//!   3. Token never stored as JS global - only in Rust `AppState`, fed via
//!      closure-private `sendIpc` in the early runtime IIFE
//!   4. `getCredentials()` / `getAuthHeaders()` removed from `@luna/lib` public API

/// Escape a string for safe embedding in a JS single-quoted string literal.
pub(super) fn escape_js(s: &str) -> String {
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
/// `plugin_id` - unique identifier (URL) for the plugin, used for storage namespace & audit.
/// `code` - already transpiled+bundled JS (no ES module syntax, ready for eval).
///
/// Returns a self-executing JS string safe to pass to `frame.execute_java_script()`.
pub fn wrap_plugin_code(
    plugin_id: &str,
    code: &str,
    load_id: u64,
    nonce: u64,
    capability: &str,
) -> String {
    let escaped_id = escape_js(plugin_id);
    // Escaped like the id. `random_capability` emits hex only, but the escape keeps a future
    // generator change from injecting into the surrounding JS string literal.
    let escaped_cap = escape_js(capability);

    // Template uses string concatenation to avoid format!() escaping hell with JS braces.
    // The IIFE parameters shadow dangerous globals - they receive `undefined` as arguments.
    let mut js = String::with_capacity(code.len() + 4096);

    // Pre-build ack request string with serde_json (no JS-side JSON.stringify dependency)
    let ack_request = serde_json::to_string(&serde_json::json!({
        "channel": "jsrt.plugin_ready",
        "args": [plugin_id, load_id.to_string(), nonce.to_string()]
    }))
    .unwrap_or_default();
    let escaped_ack = escape_js(&ack_request);

    // --- Two-layer IIFE ---
    // Outer (sloppy mode): shadows `eval` and `Function` as parameters.
    // Captures Promise.prototype.then + cefQuery + nonce BEFORE plugin code runs.
    js.push_str("(function(eval, Function) {\n");
    // Capture primitives in outer scope - plugin code can't reach these (shadowed below)
    js.push_str("var __pThen = Promise.prototype.then;\n");
    js.push_str("var __ackCq = window.cefQuery;\n");
    js.push_str("var __ackReq = '");
    js.push_str(&escaped_ack);
    js.push_str("';\n");
    // Inner IIFE is async to support top-level await (plugin .mjs files are ES modules).
    // __pThen, __ackCq, __ackReq are shadowed to undefined in the inner scope.
    js.push_str("__pThen.call(\n");
    // Sensitive globals are shadowed as parameters (receive undefined at call site).
    // This only blocks bare identifiers - window.X still works (shared V8 context limitation).
    js.push_str("(async function(__pid, __cq, __gen, __cap, localStorage, sessionStorage, XMLHttpRequest, indexedDB, caches, ServiceWorker, importScripts, __pThen, __ackCq, __ackReq, __LUNAR_CAPTURED_TOKEN__, __TIDALUNAR_CREDENTIALS__, __LUNAR_SEND_IPC__, __LUNAR_INVOKE_IPC__, __LUNAR_IPC_LISTENERS__, __LUNAR_IPC_ON__, __LUNAR_IPC_EMIT__, __LUNAR_CONFIG__, __LUNAR_SESSION_DELEGATE__, nativeInterface, cefQuery) {\n");
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
            request: JSON.stringify({ channel: 'plugin.fetch', args: [__pid, url, JSON.stringify({ method: method, headers: headers, body: opts.body || null })], id: String(++fetch.__seq), cap: __cap }),
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
            onFailure: function(code, msg) {
                var err = new Error(msg);
                err.code = code;
                reject(err);
            }
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
                request: JSON.stringify({ channel: 'plugin.storage.get', args: [__pid, key], id: String(++__idbKeyval.__seq), cap: __cap }),
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
                request: JSON.stringify({ channel: 'plugin.storage.set', args: [__pid, key, JSON.stringify(value)], id: String(++__idbKeyval.__seq), cap: __cap }),
                onSuccess: function() { resolve(); },
                onFailure: function(code, msg) { reject(new Error(msg)); }
            });
        });
    },
    del: function(key) {
        return new Promise(function(resolve, reject) {
            __cq({
                request: JSON.stringify({ channel: 'plugin.storage.del', args: [__pid, key], id: String(++__idbKeyval.__seq), cap: __cap }),
                onSuccess: function() { resolve(); },
                onFailure: function(code, msg) { reject(new Error(msg)); }
            });
        });
    },
    keys: function() {
        return new Promise(function(resolve, reject) {
            __cq({
                request: JSON.stringify({ channel: 'plugin.storage.keys', args: [__pid], id: String(++__idbKeyval.__seq), cap: __cap }),
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
    // Only expose Settings - the sole export consumed by the bundle (extractSettings).
    // Other exports (unloads, internal state, functions) stay private to the IIFE scope.
    if (__exports.Settings) window.__pluginExports[__pid] = { Settings: __exports.Settings };
    if (__exports.unloads instanceof Set) {
        for (var __fn of __exports.unloads) __unloads.push(__fn);
    }
}
"#,
    );

    // --- Inner IIFE close ---
    // Arguments: pluginId, cefQuery ref, load_id, capability, then undefined for all shadowed
    // globals.
    // 10 original shadows (localStorage..importScripts, __pThen, __ackCq, __ackReq)
    // + 11 security shadows (__LUNAR_CAPTURED_TOKEN__..__LUNAR_SESSION_DELEGATE__, nativeInterface, cefQuery)
    js.push_str("})('");
    js.push_str(&escaped_id);
    js.push_str("', window.cefQuery, ");
    js.push_str(&load_id.to_string());
    js.push_str(", '");
    js.push_str(&escaped_cap);
    js.push('\'');
    js.push_str(
        ", undefined, undefined, undefined, undefined, undefined, undefined, undefined\
         , undefined, undefined, undefined\
         , undefined, undefined, undefined, undefined, undefined, undefined, undefined, undefined, undefined, undefined, undefined)",
    );

    // --- Plugin ready ack (anti-spoofing renforcé, NOT a full security boundary) ---
    // Sent via .then() in the OUTER scope after the async IIFE fully resolves.
    // Uses captured __pThen (original Promise.prototype.then), __ackCq (original cefQuery),
    // and __ackReq (pre-serialized by Rust). All three are shadowed to undefined in the
    // inner IIFE, so plugin code cannot access or forge them.
    // NOTE: This protects against self-spoofing by the current plugin. It does NOT protect
    // against cross-plugin spoofing (a previously loaded plugin could have patched globals
    // before this wrapper captured them). Full isolation would require separate V8 isolates.
    js.push_str(",\nfunction() {\n");
    js.push_str("__ackCq({ request: __ackReq, onSuccess: function(){}, onFailure: function(c,m){ console.error('[plugin:ready]',c,m); } });\n");
    js.push_str("});\n");

    // --- Outer IIFE close ---
    // Passes undefined for eval and Function
    js.push_str("})(undefined, undefined);\n");

    js
}

#[cfg(test)]
#[path = "../../tests/unit/plugins/wrapper.rs"]
mod tests;
