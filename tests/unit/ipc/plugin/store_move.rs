//! Tests for `src/ipc/plugin/store_move.rs`, attached to it by `#[path]`.

use super::*;

fn row(url: &str, name: &str, deps: &[&str]) -> EnabledPlugin {
    let deps: Vec<_> = deps
        .iter()
        .map(|d| serde_json::json!({ "name": d, "storeUrl": "https://store" }))
        .collect();
    EnabledPlugin {
        url: url.to_string(),
        name: name.to_string(),
        code: String::new(),
        manifest: serde_json::json!({ "name": name, "luna": { "dependencies": deps } }).to_string(),
    }
}

/// A plugin depending on a held one waits with it, through a chain listed in any order: the pass
/// would otherwise disable it for an unresolved dependency. A plugin missing another dependency
/// still fails.
#[test]
fn a_dependant_of_a_held_plugin_waits_and_another_fails() {
    let unresolved = [
        row("https://a/chain", "chain", &["direct"]),
        row("https://a/direct", "direct", &["lib"]),
        row("https://b/orphan", "orphan", &["missing"]),
    ];
    let waiting = waiting_on(&unresolved, &HashSet::from(["lib".to_string()]));
    let mut urls: Vec<&str> = waiting.keys().map(String::as_str).collect();
    urls.sort_unstable();
    assert_eq!(urls, ["https://a/chain", "https://a/direct"]);
}

/// A plugin stays on behind an idle one only when every dependency it misses is idle or kept on
/// itself, through a chain listed in any order. One also missing a dead dependency fails, and so
/// does a cycle hanging off an idle name.
#[test]
fn only_a_plugin_missing_nothing_but_idle_ones_stays_on() {
    let unresolved = [
        row("https://a/chain", "chain", &["direct"]),
        row("https://a/direct", "direct", &["idle", "lib"]),
        row("https://b/mixed", "mixed", &["idle", "dead"]),
        row("https://c/one", "one", &["idle", "two"]),
        row("https://c/two", "two", &["one"]),
    ];
    let blocked = blocked_behind(
        &unresolved,
        &HashSet::from(["lib".to_string()]),
        &HashSet::from(["idle".to_string()]),
    );
    let mut urls: Vec<&str> = blocked.keys().map(String::as_str).collect();
    urls.sort_unstable();
    assert_eq!(urls, ["https://a/chain", "https://a/direct"]);
}
