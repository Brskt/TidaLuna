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
        "plugin.fetch",
        "__Luna.registerNative",
    ] {
        assert!(
            is_plugin_attributed(channel),
            "{channel} must require attribution"
        );
    }
}
