//! Tests for `src/plugins/wrapper.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn test_wrap_produces_iife() {
    let result = wrap_plugin_code(
        "https://example.com/plugin.mjs",
        "console.log('hello');",
        0,
        0,
    );
    assert!(result.starts_with("(function("));
    assert!(result.contains("'use strict'"));
    assert!(result.contains("console.log('hello');"));
    assert!(result.trim_end().ends_with(");\n") || result.trim_end().ends_with(");"));
}

#[test]
fn test_wrap_shadows_localstorage() {
    let result = wrap_plugin_code("test", "", 0, 0);
    // localStorage should be a parameter name (shadowed to undefined)
    assert!(result.contains("localStorage"));
    // The IIFE call should pass undefined for it
    assert!(result.contains(", undefined, undefined,"));
}

#[test]
fn test_wrap_contains_controlled_fetch() {
    let result = wrap_plugin_code("test", "", 0, 0);
    assert!(result.contains("var fetch = function("));
    assert!(result.contains("plugin.fetch"));
    assert!(result.contains("__cq("));
}

#[test]
fn test_wrap_contains_storage_api() {
    let result = wrap_plugin_code("test", "", 0, 0);
    assert!(result.contains("__idbKeyval"));
    assert!(result.contains("plugin.storage.get"));
    assert!(result.contains("plugin.storage.set"));
    assert!(result.contains("plugin.storage.del"));
    assert!(result.contains("plugin.storage.keys"));
}

#[test]
fn test_wrap_contains_unload_tracking() {
    let result = wrap_plugin_code("test", "", 0, 0);
    assert!(result.contains("__pluginUnloads"));
    assert!(result.contains("onUnload"));
}

#[test]
fn test_wrap_escapes_plugin_id() {
    let result = wrap_plugin_code("it's a \"test\"", "", 0, 0);
    // Single quotes escaped (embedded in JS single-quoted string)
    assert!(result.contains("it\\'s a"));
    // Double quotes pass through unescaped (safe in single-quoted JS context)
    assert!(result.contains("\"test\""));
}

#[test]
fn test_wrap_shadows_dangerous_apis() {
    let result = wrap_plugin_code("test", "", 0, 0);
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
    let result = wrap_plugin_code("test", "", 0, 0);
    // cefQuery is captured as __cq (private), not exposed by name
    assert!(result.contains("__cq"));
    assert!(result.contains("window.cefQuery"));
}
