//! What the page keeps of a plugin whose loads and unloads arrive in any order, attached to
//! `src/plugins/wrapper.rs` by `#[path]`. The real texts and `early-runtime/plugin_loads.js`
//! run under Bun in `load_orders.cjs`; this generates them and reports its verdict.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::plugins::PluginManager;

use super::wrap_plugin_code;

const URL: &str = "https://example.test/plugin's/a.mjs";

/// Bun from `dist/` when built, as `host_script.rs` resolves it, else from PATH.
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
