//! Tests for `src/plugins/wrapper.rs`, attached to it by `#[path]`.

use super::*;
use crate::plugins::PluginManager;
use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
fn test_wrap_contains_controlled_fetch() {
    let result = wrap_plugin_code("test", "", 0, 0, "cap");
    assert!(result.contains("var fetch = function("));
    assert!(result.contains("plugin.fetch"));
    assert!(result.contains("__cq("));
}

/// A single quote is escaped, the value being embedded in a JS single-quoted string; double quotes
/// pass through, which is safe in that context.
#[test]
fn a_single_quote_is_escaped_in_the_plugin_id_and_in_the_capability() {
    let rows: [(&str, &str, &[&str]); 2] = [
        ("it's a \"test\"", "cap", &["it\\'s a", "\"test\""]),
        ("test", "it's", &["it\\'s"]),
    ];
    for (id, cap, expected) in rows {
        let result = wrap_plugin_code(id, "", 0, 0, cap);
        for needle in expected {
            assert!(
                result.contains(needle),
                "{needle} missing for id {id:?}, capability {cap:?}"
            );
        }
    }
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
        // `cefQuery` is pinned by `test_wrap_cefquery_private`: `window.cefQuery` would match here.
    ] {
        assert!(result.contains(name), "Missing shadowed global: {}", name);
    }
}

/// `__cq` is the real `window.cefQuery`, handed in by position, and the bare `cefQuery` a plugin can
/// name is shadowed to `undefined`. Pairing each parameter with its argument is what pins both: a
/// text search also finds `window.cefQuery` in the ack captured outside the IIFE.
#[test]
fn test_wrap_cefquery_private() {
    let result = wrap_plugin_code("test", "", 0, 0, "cap");
    let params: Vec<&str> = result
        .split("(async function(")
        .nth(1)
        .and_then(|s| s.split(')').next())
        .expect("parameter list present")
        .split(',')
        .map(str::trim)
        .collect();
    let args: Vec<&str> = result
        .rsplit("})('")
        .next()
        .and_then(|s| s.split(')').next())
        .expect("argument list present")
        .split(',')
        .map(str::trim)
        .collect();
    let bound = |name: &str| params.iter().position(|p| *p == name).map(|i| args[i]);

    assert_eq!(bound("__cq"), Some("window.cefQuery"));
    assert_eq!(bound("cefQuery"), Some("undefined"));
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
    // and storage shims above do it rather than by reaching for a global. Both bridge methods
    // carry it: the shims hold `cap: __cap` too, a bare search would find theirs.
    assert!(result.contains(
        "request: JSON.stringify({ channel: channel, args: args, id: String(++__ipcRenderer.__seq), cap: __cap })"
    ));
    assert!(
        result.contains("request: JSON.stringify({ channel: channel, args: args, cap: __cap })")
    );
    // Listener registration acts on no caller's identity and keeps forwarding; `on` still
    // returns the unsubscribe the page's object hands back.
    assert!(result.contains("window.__ipcRenderer"));
}

// What the page keeps of a plugin whose loads and unloads arrive in any order. The real texts and
// `early-runtime/plugin_loads.js` run under Bun in `load_orders.cjs`; this generates them and
// reports its verdict.

const URL: &str = "https://example.test/plugin's/a.mjs";

/// Bun from `dist/` when built, as `tests/unit/native_runtime.rs` resolves it, else from PATH.
fn bun_path(root: &Path) -> PathBuf {
    let name = if cfg!(windows) { "bun.exe" } else { "bun" };
    crate::native_runtime::find_binary(&root.join("dist"), name)
        .unwrap_or_else(|| PathBuf::from(name))
}

/// A plugin that only says whether it is running.
fn load_text(load_id: u64) -> String {
    let body = format!(
        "globalThis.__live.add({load_id});\nonUnload(() => globalThis.__live.delete({load_id}));"
    );
    wrap_plugin_code(URL, &body, load_id, 7, "cap")
}

/// Restart: a disable unloads load 1, an enable mints 2, a restart reads 2 as live, unloads it and
/// mints 3, and the enable's code can land after that unload. Mirror: a restart's post-check
/// unloads its own load 3 after an enable minted 4. The manager ends on 3, then on 4.
#[test]
fn a_plugin_runs_once_as_the_managers_load_in_every_order() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let texts = serde_json::json!({
        "load": (1..=4)
            .map(|id| (id.to_string(), load_text(id).into()))
            .collect::<serde_json::Map<_, _>>(),
        "unload": (1..=4)
            .map(|id| (id.to_string(), PluginManager::generate_unload_js(URL, id).into()))
            .collect::<serde_json::Map<_, _>>(),
    });
    let dir = tempfile::tempdir().expect("cannot create a scratch dir");
    let texts_path = dir.path().join("texts.json");
    std::fs::write(&texts_path, texts.to_string()).expect("cannot write the texts");

    let bun = bun_path(&root);
    let out = Command::new(&bun)
        .arg(root.join("tests/unit/plugins/load_orders.cjs"))
        .arg(&texts_path)
        .arg(root.join("frontend/early-runtime/plugin_loads.js"))
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "cannot run {}: {e}\nthe page side is JavaScript, so this test needs Bun - \
                 either build dist/ or put bun on PATH",
                bun.display()
            )
        });
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{report}");
}
