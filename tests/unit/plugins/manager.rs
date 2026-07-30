//! Tests for `src/plugins/manager.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn test_transpile_and_wrap_wraps_code() {
    let result =
        PluginManager::transpile_and_wrap("test-plugin", "console.log('hello');", 0, 0).unwrap();

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
fn random_nonce_is_none_on_entropy_failure() {
    assert_eq!(random_nonce_with(|_| false), None);
}

#[test]
fn random_nonce_decodes_le_bytes() {
    let n = random_nonce_with(|b| {
        b.copy_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
        true
    });
    assert_eq!(n, Some(1));
}

#[test]
fn test_mark_loading_returns_unique_load_id() {
    let mut mgr = PluginManager::new();
    let id1 = mgr.mark_loading("a", "a", 1);
    let id2 = mgr.mark_loading("b", "b", 2);
    assert_ne!(id1, id2);
    assert!(mgr.is_loaded("a"));
    assert!(mgr.is_loaded("b"));
}

#[test]
fn test_mark_ready_with_matching_load_id_and_nonce() {
    let mut mgr = PluginManager::new();
    let nonce = 0xABCD;
    let load_id = mgr.mark_loading("p", "p", nonce);
    assert!(!mgr.is_ready("p"));
    assert!(mgr.mark_ready("p", load_id, nonce));
    assert!(mgr.is_ready("p"));
    assert!(mgr.is_loaded("p"));
}

#[test]
fn test_mark_ready_with_stale_load_id_ignored() {
    let mut mgr = PluginManager::new();
    let old_nonce = 0x1111;
    let old_id = mgr.mark_loading("p", "p", old_nonce);
    let _new_id = mgr.mark_loading("p", "p", 0x2222); // reload
    assert!(!mgr.mark_ready("p", old_id, old_nonce)); // stale
    assert!(!mgr.is_ready("p"));
}

#[test]
fn test_mark_ready_with_wrong_nonce_rejected() {
    let mut mgr = PluginManager::new();
    let load_id = mgr.mark_loading("p", "p", 0x4242);
    assert!(!mgr.mark_ready("p", load_id, 99999)); // wrong nonce
    assert!(!mgr.is_ready("p"));
}

#[test]
fn test_is_loaded_during_loading() {
    let mut mgr = PluginManager::new();
    mgr.mark_loading("p", "p", 0);
    assert!(mgr.is_loaded("p"));
    assert!(!mgr.is_ready("p"));
}

#[test]
fn test_mark_unloaded_clears_state() {
    let mut mgr = PluginManager::new();
    mgr.mark_loading("p", "p", 0);
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
