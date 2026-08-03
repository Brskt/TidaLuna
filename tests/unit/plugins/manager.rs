//! Tests for `src/plugins/manager.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn test_transpile_and_wrap_wraps_code() {
    let result =
        PluginManager::transpile_and_wrap("test-plugin", "console.log('hello');", 0, 0, "cap")
            .unwrap();

    assert!(result.starts_with("(function("));
    assert!(result.contains("console.log("));
    assert!(result.contains("hello"));
    assert!(result.contains("'use strict'"));
}

#[test]
fn test_transpile_and_wrap_transpiles_ts() {
    let ts_code = "const x: number = 42; console.log(x);";
    let result = PluginManager::transpile_and_wrap("ts-plugin", ts_code, 0, 0, "cap").unwrap();

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
    let id1 = mgr.mark_loading("a", "a", 1, "cap-a");
    let id2 = mgr.mark_loading("b", "b", 2, "cap-b");
    assert_ne!(id1, id2);
    assert!(mgr.is_loaded("a"));
    assert!(mgr.is_loaded("b"));
}

#[test]
fn test_mark_ready_with_matching_load_id_and_nonce() {
    let mut mgr = PluginManager::new();
    let nonce = 0xABCD;
    let load_id = mgr.mark_loading("p", "p", nonce, "cap-p");
    assert!(!mgr.is_ready("p"));
    assert!(mgr.mark_ready("p", load_id, nonce));
    assert!(mgr.is_ready("p"));
    assert!(mgr.is_loaded("p"));
}

#[test]
fn test_mark_ready_with_stale_load_id_ignored() {
    let mut mgr = PluginManager::new();
    let old_nonce = 0x1111;
    let old_id = mgr.mark_loading("p", "p", old_nonce, "cap-old");
    let _new_id = mgr.mark_loading("p", "p", 0x2222, "cap-new"); // reload
    assert!(!mgr.mark_ready("p", old_id, old_nonce)); // stale
    assert!(!mgr.is_ready("p"));
}

#[test]
fn test_mark_ready_with_wrong_nonce_rejected() {
    let mut mgr = PluginManager::new();
    let load_id = mgr.mark_loading("p", "p", 0x4242, "cap-p");
    assert!(!mgr.mark_ready("p", load_id, 99999)); // wrong nonce
    assert!(!mgr.is_ready("p"));
}

#[test]
fn test_is_loaded_during_loading() {
    let mut mgr = PluginManager::new();
    mgr.mark_loading("p", "p", 0, "cap-p");
    assert!(mgr.is_loaded("p"));
    assert!(!mgr.is_ready("p"));
}

#[test]
fn test_mark_unloaded_clears_state() {
    let mut mgr = PluginManager::new();
    mgr.mark_loading("p", "p", 0, "cap-p");
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

#[test]
fn random_capability_is_none_on_entropy_failure() {
    assert_eq!(random_capability_with(|_| false), None);
}

#[test]
fn random_capability_is_lowercase_hex_of_the_bytes() {
    let cap = random_capability_with(|b| {
        b.fill(0);
        b[0] = 0xAB;
        true
    })
    .expect("entropy available");
    assert!(cap.starts_with("ab00"), "got {cap}");
    assert_eq!(cap.len(), 64, "32 bytes hex-encoded");
}

#[test]
fn a_capability_resolves_to_the_plugin_it_was_issued_for() {
    let mut mgr = PluginManager::new();
    mgr.mark_loading("https://example.test/a.js", "A", 1, "cap-a");
    assert_eq!(
        mgr.plugin_for_capability("cap-a").as_deref(),
        Some("https://example.test/a.js")
    );
}

#[test]
fn a_capability_resolves_only_to_its_own_plugin() {
    let mut mgr = PluginManager::new();
    mgr.mark_loading("https://example.test/a.js", "A", 1, "cap-a");
    mgr.mark_loading("https://example.test/b.js", "B", 2, "cap-b");
    assert_eq!(
        mgr.plugin_for_capability("cap-b").as_deref(),
        Some("https://example.test/b.js")
    );
}

#[test]
fn an_unknown_capability_resolves_to_nothing() {
    let mgr = PluginManager::new();
    assert!(mgr.plugin_for_capability("never-issued").is_none());
    assert!(mgr.plugin_for_capability("").is_none());
}

/// `eval_js` dispatches the cleanup: `onUnload` runs after `mark_unloaded` returns; revoking here
/// refused the settings write those handlers exist to make.
#[test]
fn unloading_leaves_the_capability_usable_for_the_unload_handler() {
    let mut mgr = PluginManager::new();
    mgr.mark_loading("https://example.test/a.js", "A", 1, "cap-a");
    mgr.mark_unloaded("https://example.test/a.js");
    assert_eq!(
        mgr.plugin_for_capability("cap-a").as_deref(),
        Some("https://example.test/a.js"),
        "the unload handler still needs to be attributable"
    );
}

/// `reload()` awaits `disable()` then `enable()`, and disable only dispatches the cleanup: an async
/// `onUnload` continuing after an await still needs to be attributable.
#[test]
fn reloading_leaves_the_previous_capability_usable() {
    let mut mgr = PluginManager::new();
    mgr.mark_loading("https://example.test/a.js", "A", 1, "cap-old");
    mgr.mark_loading("https://example.test/a.js", "A", 2, "cap-new");
    for cap in ["cap-old", "cap-new"] {
        assert_eq!(
            mgr.plugin_for_capability(cap).as_deref(),
            Some("https://example.test/a.js"),
            "{cap}"
        );
    }
}

/// Bounded per plugin, not globally: a plugin reloading in a loop must not be able to evict another
/// plugin's capability and turn its storage writes into 403s.
#[test]
fn reloads_beyond_the_bound_drop_only_that_plugins_oldest() {
    let mut mgr = PluginManager::new();
    mgr.mark_loading("https://example.test/other.js", "Other", 0, "cap-other");
    for i in 0..=MAX_CAPABILITIES_PER_PLUGIN {
        mgr.mark_loading(
            "https://example.test/a.js",
            "A",
            i as u64,
            &format!("cap-{i}"),
        );
    }

    assert!(
        mgr.plugin_for_capability("cap-0").is_none(),
        "its own oldest is dropped"
    );
    assert_eq!(
        mgr.plugin_for_capability(&format!("cap-{MAX_CAPABILITIES_PER_PLUGIN}"))
            .as_deref(),
        Some("https://example.test/a.js"),
        "the newest survives"
    );
    assert_eq!(
        mgr.plugin_for_capability("cap-other").as_deref(),
        Some("https://example.test/other.js"),
        "another plugin's capability is untouched"
    );
}
