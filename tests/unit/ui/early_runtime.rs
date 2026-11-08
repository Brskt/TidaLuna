//! Tests for `src/ui/early_runtime.rs`, attached by `#[path]`.

use crate::frontend::load::{EarlyRuntime, Frontend};
use crate::ui::nav::{NavigationPolicy, PageKind};

use super::{COVERED_KINDS, payload_from_frontend, preload_script};

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
            token_capture: s("token_capture"),
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

#[test]
fn the_preload_runs_the_scripts_in_this_order() {
    let js = preload_script(&payload_from_frontend(&frontend()), false, true);
    let order = [
        "self.__LUNAR_CONFIG__=",
        "/*host_modules*/",
        "/*saga_put*/",
        "/*ipc*/",
        "/*volume_restore*/",
        "/*token_capture*/",
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

/// Under a system frame the strip shows only where TIDAL's shell never loads: the value must be the
/// one the bundle injection itself follows, or the login page loses its way back.
#[test]
fn the_preload_says_whether_the_page_gets_the_bundle() {
    let payload = payload_from_frontend(&frontend());
    for kind in COVERED_KINDS {
        let bundle = NavigationPolicy::for_page(kind).inject_bundle;
        let js = preload_script(&payload, false, bundle);
        assert!(
            js.contains(&format!("self.__LUNAR_PAGE_HAS_BUNDLE__={bundle};")),
            "{kind:?}"
        );
    }
    assert!(
        !NavigationPolicy::for_page(PageKind::AuthHost).inject_bundle,
        "the login page is the one without it"
    );
}

#[test]
fn the_payload_carries_no_credential() {
    let payload = payload_from_frontend(&frontend());
    assert!(!payload.body.contains("__TIDALUNAR_CREDENTIALS__"));
    assert!(!payload.body.contains("/*init_static*/"));
    assert!(!payload.body.contains("/*cache_heal*/"));
}

#[test]
fn every_injecting_page_kind_has_a_sender() {
    for kind in [
        PageKind::DesktopApp,
        PageKind::LoginCallback,
        PageKind::LoginPage,
        PageKind::AuthHost,
        PageKind::TidalCallback,
        PageKind::External,
    ] {
        if NavigationPolicy::for_page(kind).inject_early_runtime {
            assert!(
                COVERED_KINDS.contains(&kind),
                "{kind:?} injects but no sender covers it"
            );
        }
    }
}
