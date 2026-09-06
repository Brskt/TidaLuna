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

/// What a `registerNative` claim is checked against. Keyed by url, because that is the unique
/// half: two plugins may declare the same manifest name, and keyed by the name the second to load
/// would take the entry and leave the first unable to register anything at all.
#[test]
fn the_name_a_url_declared_is_what_answers_for_it() {
    let mut mgr = PluginManager::new();
    mgr.mark_loading("https://a.invalid/a.mjs", "Shared", 1, "cap-a", 0);
    mgr.mark_loading("https://b.invalid/b.mjs", "Shared", 2, "cap-b", 0);

    assert_eq!(mgr.name_for_url("https://a.invalid/a.mjs"), Some("Shared"));
    assert_eq!(mgr.name_for_url("https://b.invalid/b.mjs"), Some("Shared"));
    assert_eq!(mgr.name_for_url("https://c.invalid/c.mjs"), None);

    // Uninstalled, the url answers for nothing: a later claim on its name must not be served from
    // a plugin that is gone.
    mgr.forget_plugin("https://a.invalid/a.mjs");
    assert_eq!(mgr.name_for_url("https://a.invalid/a.mjs"), None);
    assert_eq!(mgr.name_for_url("https://b.invalid/b.mjs"), Some("Shared"));
}

/// Ending a session names the loads it drops in the same breath as dropping them. Listing
/// and retiring as two steps reopens the window the bulk form exists to close: the caller
/// hands the lock back between them, and a plugin registering in that gap is in neither the
/// list the caller cleans up nor the retirement. The injection it is about to post
/// outlives the session with nothing left naming it.
#[test]
fn retiring_every_load_names_exactly_what_it_dropped() {
    let mut mgr = PluginManager::new();
    let id_a = mgr.mark_loading("https://a.invalid/a.mjs", "A", 1, "cap-a", 0);
    let id_b = mgr.mark_loading("https://b.invalid/b.mjs", "B", 2, "cap-b", 0);

    let retired = mgr.retire_all_loaded();

    assert!(
        mgr.loaded_loads().is_empty(),
        "a load left behind survives the epoch bump that follows, and nothing else names it"
    );
    // The pair, not the url alone: a cleanup carrying no load id reaches past the session it
    // is ending into whatever load became current after it.
    assert!(
        retired.contains(&("https://a.invalid/a.mjs".to_string(), id_a)),
        "the first load went unnamed, got {retired:?}"
    );
    assert!(
        retired.contains(&("https://b.invalid/b.mjs".to_string(), id_b)),
        "the second load went unnamed, got {retired:?}"
    );
}

#[test]
fn test_mark_loading_returns_unique_load_id() {
    let mut mgr = PluginManager::new();
    let id1 = mgr.mark_loading("a", "a", 1, "cap-a", 0);
    let id2 = mgr.mark_loading("b", "b", 2, "cap-b", 0);
    assert_ne!(id1, id2);
    assert_eq!(mgr.current_load_id("a"), Some(id1));
    assert_eq!(mgr.current_load_id("b"), Some(id2));
}

/// Readiness is read through the transition rather than through a field: `mark_ready` only
/// matches a LOADING entry; a second one finding nothing to promote is the manager saying
/// the first moved the state. That pins the machine, where a peek only pinned a value.
#[test]
fn test_mark_ready_with_matching_load_id_and_nonce() {
    let mut mgr = PluginManager::new();
    let nonce = 0xABCD;
    let load_id = mgr.mark_loading("p", "p", nonce, "cap-p", 0);
    assert!(
        mgr.mark_ready("p", load_id, nonce),
        "an ack naming its own load is accepted"
    );
    assert!(
        !mgr.mark_ready("p", load_id, nonce),
        "and the entry has left Loading: a second ack has nothing to promote"
    );
    assert_eq!(mgr.current_load_id("p"), Some(load_id));
}

#[test]
fn test_mark_ready_with_stale_load_id_ignored() {
    let mut mgr = PluginManager::new();
    let old_nonce = 0x1111;
    let old_id = mgr.mark_loading("p", "p", old_nonce, "cap-old", 0);
    let new_id = mgr.mark_loading("p", "p", 0x2222, "cap-new", 0); // reload
    assert!(!mgr.mark_ready("p", old_id, old_nonce)); // stale
    assert_eq!(
        mgr.current_load_id("p"),
        Some(new_id),
        "the reload is untouched by an ack for the load it replaced"
    );
}

#[test]
fn test_mark_ready_with_wrong_nonce_rejected() {
    let mut mgr = PluginManager::new();
    let load_id = mgr.mark_loading("p", "p", 0x4242, "cap-p", 0);
    assert!(!mgr.mark_ready("p", load_id, 99999)); // wrong nonce
    assert!(
        mgr.mark_ready("p", load_id, 0x4242),
        "the entry is still Loading, so the right nonce is still accepted"
    );
}

#[test]
fn a_loading_plugin_reports_the_load_that_owns_it() {
    let mut mgr = PluginManager::new();
    let load_id = mgr.mark_loading("p", "p", 0, "cap-p", 0);
    assert_eq!(mgr.current_load_id("p"), Some(load_id));
    assert_eq!(mgr.current_load_id("absent"), None);
}

#[test]
fn retiring_a_load_clears_state() {
    let mut mgr = PluginManager::new();
    let load_id = mgr.mark_loading("p", "p", 0, "cap-p", 0);
    assert!(mgr.retire_load("p", load_id));
    assert_eq!(mgr.current_load_id("p"), None);
}

/// Nothing serialises `plugin.enable` against `plugin.disable`, and a disable holds no lock
/// across its DB round trip. The removal therefore has to name the load it is ending.
///
/// Unconditional, it erased whatever entry happened to be there. A disable that lost the
/// race dropped the `Loading` a newer enable had just written, while that enable went on to
/// inject its code regardless. The plugin then ran in the renderer with no map knowing it: no
/// sweep unloads it, and the ten-second watchdog looks it up, finds nothing, and concludes
/// there is nothing to rescue.
#[test]
fn retiring_a_stale_load_leaves_a_newer_one_standing() {
    let mut mgr = PluginManager::new();
    let old = mgr.mark_loading("p", "p", 1, "cap-old", 0);
    let new = mgr.mark_loading("p", "p", 2, "cap-new", 0);

    assert!(
        !mgr.retire_load("p", old),
        "the disable arrived late: the load it meant to end is already gone"
    );
    assert_eq!(
        mgr.current_load_id("p"),
        Some(new),
        "and the load that replaced it is still tracked, which is what stops it becoming a ghost"
    );

    assert!(mgr.retire_load("p", new), "its own load still retires");
    assert_eq!(mgr.current_load_id("p"), None);
}

/// The watchdogs fire on a load that never acked and must not touch one that did. An ack
/// landing between a separate read and removal is exactly the window this closes.
#[test]
fn a_watchdog_abandons_only_a_load_still_waiting_for_its_ack() {
    let mut mgr = PluginManager::new();
    let load_id = mgr.mark_loading("p", "p", 7, "cap-p", 0);
    assert!(mgr.mark_ready("p", load_id, 7));

    assert!(
        !mgr.abandon_if_still_loading("p", load_id),
        "the ack landed first: tearing this down would destroy a plugin that works"
    );
    assert_eq!(
        mgr.current_load_id("p"),
        Some(load_id),
        "and it is left exactly as it was"
    );

    let second = mgr.mark_loading("q", "q", 8, "cap-q", 0);
    assert!(
        mgr.abandon_if_still_loading("q", second),
        "one that never acked is still the watchdog's to abandon"
    );
    assert_eq!(mgr.current_load_id("q"), None);
}

/// Uninstall is the one removal that names no load: the DB row is gone, and a conditional
/// removal that lost a race would strand an entry for a plugin nothing can reach again.
#[test]
fn forgetting_a_plugin_ignores_which_load_is_current() {
    let mut mgr = PluginManager::new();
    mgr.mark_loading("p", "p", 1, "cap-old", 0);
    mgr.mark_loading("p", "p", 2, "cap-new", 0);

    mgr.forget_plugin("p");

    assert_eq!(mgr.current_load_id("p"), None);
    assert!(mgr.loaded_loads().is_empty());
}

/// The sweep pairs each url with its load in ONE read. Taken as two (a list of urls, then a
/// lookup each), the pair can describe two different loads, the same race one layer up.
#[test]
fn the_loaded_snapshot_carries_each_plugins_load() {
    let mut mgr = PluginManager::new();
    let a = mgr.mark_loading("a", "A", 1, "cap-a", 0);
    let b = mgr.mark_loading("b", "B", 2, "cap-b", 0);

    let mut snapshot = mgr.loaded_loads();
    snapshot.sort();
    assert_eq!(
        snapshot,
        vec![("a".to_string(), a), ("b".to_string(), b)],
        "each url arrives with the load that owns it"
    );
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
    mgr.mark_loading("https://example.test/a.js", "A", 1, "cap-a", 0);
    assert_eq!(
        mgr.plugin_for_capability("cap-a").as_deref(),
        Some("https://example.test/a.js")
    );
}

#[test]
fn a_capability_resolves_only_to_its_own_plugin() {
    let mut mgr = PluginManager::new();
    mgr.mark_loading("https://example.test/a.js", "A", 1, "cap-a", 0);
    mgr.mark_loading("https://example.test/b.js", "B", 2, "cap-b", 0);
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

/// `eval_js` dispatches the cleanup: `onUnload` runs after the load is retired; revoking here
/// refused the settings write those handlers exist to make.
#[test]
fn unloading_leaves_the_capability_usable_for_the_unload_handler() {
    let mut mgr = PluginManager::new();
    let load_id = mgr.mark_loading("https://example.test/a.js", "A", 1, "cap-a", 0);
    mgr.retire_load("https://example.test/a.js", load_id);
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
    mgr.mark_loading("https://example.test/a.js", "A", 1, "cap-old", 0);
    mgr.mark_loading("https://example.test/a.js", "A", 2, "cap-new", 0);
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
    mgr.mark_loading("https://example.test/other.js", "Other", 0, "cap-other", 0);
    for i in 0..=MAX_CAPABILITIES_PER_PLUGIN {
        mgr.mark_loading(
            "https://example.test/a.js",
            "A",
            i as u64,
            &format!("cap-{i}"),
            0,
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

/// A capability records the session it was minted under, so the one reader that hands out a
/// live token can tell a capability from this session from one that outlived its own.
///
/// The window this closes: a plugin activated as a logout lands gets its code into the
/// renderer anyway, and nothing tracks it afterwards; it is in no `loaded_urls()`, so no
/// later sweep unloads it. Its capability still resolves, which is deliberate, and until this
/// it still bought the NEXT session's access token.
#[test]
fn a_capability_remembers_the_session_that_issued_it() {
    let mut mgr = PluginManager::new();
    mgr.mark_loading("https://example.test/a.js", "A", 1, "cap-old", 4);
    mgr.mark_loading("https://example.test/b.js", "B", 2, "cap-now", 5);

    assert_eq!(mgr.capability_epoch("cap-old"), Some(4));
    assert_eq!(mgr.capability_epoch("cap-now"), Some(5));
    assert_eq!(
        mgr.capability_epoch("never-issued"),
        None,
        "a capability nobody minted names no session"
    );
    assert_eq!(
        mgr.capability_epoch(""),
        None,
        "an empty envelope field cannot match, here as in attribution"
    );
}

/// The guard above must not become an attribution check. Retiring a load keeps its capability
/// alive on purpose: `eval_js` only DISPATCHES the cleanup, so a plugin's `onUnload` runs
/// afterwards and an async one continues past an await. Its settings write still has to be
/// attributable, including when the unload was a logout sweep.
///
/// This is the regression this whole design avoids: refusing a past-epoch capability at the
/// point where the CALLER is resolved would 403 that write for every plugin loaded at logout,
/// not just a ghost.
#[test]
fn a_capability_from_a_past_session_still_names_its_plugin() {
    let mut mgr = PluginManager::new();
    let load_id = mgr.mark_loading("https://example.test/a.js", "A", 1, "cap-a", 4);
    mgr.retire_load("https://example.test/a.js", load_id);

    assert_eq!(
        mgr.plugin_for_capability("cap-a").as_deref(),
        Some("https://example.test/a.js"),
        "the unload handler is still attributable, which is why the capability outlives it"
    );
    assert_eq!(
        mgr.capability_epoch("cap-a"),
        Some(4),
        "and the session it belonged to is still readable, for the credential check alone"
    );
}
