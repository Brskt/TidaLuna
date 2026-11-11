//! Tests for `src/ipc/plugin/uninstall.rs`, attached to it by `#[path]`.

use super::{CleanupScope, cleanup_scope_for_uninstall};

/// An url the database does not know yields no canonical name. Read as the `uninstall_all` sentinel,
/// that cleared every plugin's trust rows, native channel tokens and capabilities (reachable by any
/// plugin, and even by an argument-less call).
#[test]
fn an_uninstall_clears_only_the_plugin_it_matched() {
    for (name, expected) in [("", None), ("DiscordRPC", Some("DiscordRPC"))] {
        let scoped = cleanup_scope_for_uninstall(name.to_string()).map(|scope| match scope {
            CleanupScope::Plugin(plugin) => plugin.as_str().to_string(),
            CleanupScope::All => "every plugin".to_string(),
        });
        assert_eq!(scoped.as_deref(), expected, "{name:?}");
    }
}
