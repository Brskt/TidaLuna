//! Tests for `src/ui/early_runtime.rs`, attached by `#[path]`.

use crate::frontend::load::{EarlyRuntime, Frontend};

use super::{payload_from_frontend, preload_script};

/// Each script names itself, letting order and presence be read off the output.
fn frontend() -> Frontend {
    let s = |name: &str| format!("/*{name}*/").into_boxed_str();
    Frontend {
        bundle: s("bundle"),
        luna_ui: Box::new([]),
        luna_dev: Box::new([]),
        native_host: s("native_host"),
        early: EarlyRuntime {
            host_modules: s("host_modules"),
            saga_put: s("saga_put"),
            ipc: s("ipc"),
            volume_restore: s("volume_restore"),
            xhr_pin: s("xhr_pin"),
            fetch_proxy: s("fetch_proxy"),
            window_open: s("window_open"),
            session_stub: s("session_stub"),
            exfil_guard: s("exfil_guard"),
            plugin_loads: s("plugin_loads"),
            cover_error: s("cover_error"),
            fallback_titlebar: s("fallback_titlebar"),
            cache_heal: s("cache_heal"),
            init_static: s("init_static"),
        },
    }
}

/// TIDAL reads `TIDAL_CONFIG` once, while its modules evaluate: `init_static` rides the payload
/// ahead of them. Sent from the browser, it lost that race after a login.
#[test]
fn the_preload_runs_the_scripts_in_this_order() {
    let js = preload_script(&payload_from_frontend(&frontend()), false, true);
    let order = [
        "self.__LUNAR_CONFIG__=",
        "/*init_static*/",
        "/*host_modules*/",
        "/*saga_put*/",
        "/*ipc*/",
        "/*volume_restore*/",
        "/*xhr_pin*/",
        "/*fetch_proxy*/",
        "/*window_open*/",
        "/*session_stub*/",
        "/*exfil_guard*/",
        "/*plugin_loads*/",
        "/*cover_error*/",
        "self.__LUNAR_PAGE_HAS_BUNDLE__=",
        "/*fallback_titlebar*/",
    ];
    let positions: Vec<usize> = order
        .iter()
        .map(|marker| {
            js.find(marker)
                .unwrap_or_else(|| panic!("{marker} missing"))
        })
        .collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{positions:?}");
}

#[test]
fn the_preload_keeps_its_guards() {
    let js = preload_script(&payload_from_frontend(&frontend()), false, true);
    assert!(
        js.contains("if(self.__LUNAR_EARLY_RUNTIME__)return;"),
        "{js}"
    );
    assert!(
        js.contains("if(typeof window.cefQuery!=='function')return;"),
        "{js}"
    );
    assert!(js.contains("self.__LUNAR_EARLY_RUNTIME__=true;"), "{js}");
}
