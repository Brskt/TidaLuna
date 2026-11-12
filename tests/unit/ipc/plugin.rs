//! Tests for `src/ipc/plugin/mod.rs`, attached to it by `#[path]`.

use super::*;

/// Each is reachable by a route that carries the capability: the wrapper's own shims for the
/// storage and fetch channels, and for `registerNative` both the wrapper's `__ipcRenderer` shadow
/// and the per-plugin `@luna/lib` the transpiler binds. Requiring it costs no plugin change.
#[test]
fn the_channels_a_plugin_must_be_known_on_are_attributed() {
    for channel in [
        "plugin.storage.get",
        "plugin.storage.set",
        "plugin.storage.del",
        "plugin.storage.keys",
    ] {
        assert!(
            matches!(attributed_route(channel), Some(AttributedRoute::Storage)),
            "{channel} must require attribution"
        );
    }
    assert!(matches!(
        attributed_route("plugin.fetch"),
        Some(AttributedRoute::Fetch)
    ));
    assert!(matches!(
        attributed_route("__Luna.registerNative"),
        Some(AttributedRoute::RegisterNative)
    ));
}

/// Served unattributed: `plugin.fetch_package` runs before any plugin holds a capability, and the
/// app bundle calls the other two with none.
#[test]
fn the_channels_served_without_a_capability_are_not_attributed() {
    for channel in ["plugin.fetch_package", "plugin.list", "tidal.fetch"] {
        assert!(
            attributed_route(channel).is_none(),
            "{channel} must not require attribution"
        );
    }
}
