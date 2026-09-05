//! Tests for `src/plugins/wrapper.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn test_wrap_produces_iife() {
    let result = wrap_plugin_code(
        "https://example.com/plugin.mjs",
        "console.log('hello');",
        0,
        0,
        "cap",
    );
    assert!(result.starts_with("(function("));
    assert!(result.contains("'use strict'"));
    assert!(result.contains("console.log('hello');"));
    assert!(result.trim_end().ends_with(");\n") || result.trim_end().ends_with(");"));
}

#[test]
fn test_wrap_shadows_localstorage() {
    let result = wrap_plugin_code("test", "", 0, 0, "cap");
    // localStorage should be a parameter name (shadowed to undefined)
    assert!(result.contains("localStorage"));
    // The IIFE call should pass undefined for it
    assert!(result.contains(", undefined, undefined,"));
}

#[test]
fn test_wrap_contains_controlled_fetch() {
    let result = wrap_plugin_code("test", "", 0, 0, "cap");
    assert!(result.contains("var fetch = function("));
    assert!(result.contains("plugin.fetch"));
    assert!(result.contains("__cq("));
}

#[test]
fn test_wrap_contains_storage_api() {
    let result = wrap_plugin_code("test", "", 0, 0, "cap");
    assert!(result.contains("__idbKeyval"));
    assert!(result.contains("plugin.storage.get"));
    assert!(result.contains("plugin.storage.set"));
    assert!(result.contains("plugin.storage.del"));
    assert!(result.contains("plugin.storage.keys"));
}

#[test]
fn test_wrap_contains_unload_tracking() {
    let result = wrap_plugin_code("test", "", 0, 0, "cap");
    assert!(result.contains("__pluginUnloads"));
    assert!(result.contains("onUnload"));
}

#[test]
fn test_wrap_escapes_plugin_id() {
    let result = wrap_plugin_code("it's a \"test\"", "", 0, 0, "cap");
    // Single quotes escaped (embedded in JS single-quoted string)
    assert!(result.contains("it\\'s a"));
    // Double quotes pass through unescaped (safe in single-quoted JS context)
    assert!(result.contains("\"test\""));
}

#[test]
fn test_wrap_shadows_dangerous_apis() {
    let result = wrap_plugin_code("test", "", 0, 0, "cap");
    // All these should be parameter names (shadowed)
    // WebSocket intentionally NOT shadowed - plugins like DiscordRPC need it.
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
        // Security shadows - bare identifiers only (window.X still accessible)
        "__LUNAR_CAPTURED_TOKEN__",
        "__TIDALUNAR_CREDENTIALS__",
        "__LUNAR_SEND_IPC__",
        "__LUNAR_INVOKE_IPC__",
        "__LUNAR_IPC_LISTENERS__",
        "__LUNAR_IPC_ON__",
        "__LUNAR_IPC_EMIT__",
        "__LUNAR_CONFIG__",
        "__LUNAR_SESSION_DELEGATE__",
        "nativeInterface",
        "cefQuery",
    ] {
        assert!(result.contains(name), "Missing shadowed global: {}", name);
    }
}

#[test]
fn test_wrap_cefquery_private() {
    let result = wrap_plugin_code("test", "", 0, 0, "cap");
    // cefQuery is captured as __cq (private), not exposed by name
    assert!(result.contains("__cq"));
    assert!(result.contains("window.cefQuery"));
}

#[test]
fn the_capability_is_a_closure_parameter_and_never_a_global() {
    let result = wrap_plugin_code("test", "", 0, 0, "CAP-TOKEN");
    assert!(result.contains("__cap"), "the parameter is emitted");
    assert!(
        result.contains("'CAP-TOKEN'"),
        "the value is passed as an argument"
    );
    // Reachable from a global, the capability would stop being one: any plugin could read it.
    assert!(!result.contains("window.__cap"));
    assert!(!result.contains("__cap ="));
}

/// The invocation pads one `undefined` per shadowed global; a miscount shifts every shadow by one and
/// hands plugin code a real value where it should see `undefined`.
#[test]
fn every_iife_parameter_still_receives_an_argument() {
    let result = wrap_plugin_code("test", "", 0, 0, "cap");
    let params = result
        .split("(async function(")
        .nth(1)
        .and_then(|s| s.split(')').next())
        .expect("parameter list present")
        .split(',')
        .count();
    let args = result
        .rsplit("})('")
        .next()
        .and_then(|s| s.split(')').next())
        .expect("argument list present")
        .split(',')
        .count();
    assert_eq!(params, args, "parameters and arguments must stay in step");
}

/// The upstream build tool inlines `__ipcRenderer.invoke("__Luna.registerNative", ...)` into a
/// plugin's own bundle, against the bare name. A shadow here is therefore the only thing that can
/// make that call carry an identity, and now that Rust refuses the channel unattributed, its
/// absence would refuse every native plugin instead of only the ones impersonating another.
#[test]
fn the_ipc_bridge_is_shadowed_and_carries_the_capability() {
    let result = wrap_plugin_code("test", "", 0, 0, "CAP-TOKEN");
    assert!(
        result.contains("var __ipcRenderer = {"),
        "the bare identifier the inlined call uses is not shadowed"
    );
    // The identity rides the envelope, taken from the closure parameter, the same way the fetch
    // and storage shims above do it rather than by reaching for a global.
    assert!(result.contains("cap: __cap"));
    // Listener registration acts on no caller's identity and keeps forwarding; `on` still
    // returns the unsubscribe the page's object hands back.
    assert!(result.contains("window.__ipcRenderer"));
}

#[test]
fn the_capability_is_escaped_like_the_plugin_id() {
    let result = wrap_plugin_code("test", "", 0, 0, "it's");
    assert!(result.contains("it\\'s"));
}
