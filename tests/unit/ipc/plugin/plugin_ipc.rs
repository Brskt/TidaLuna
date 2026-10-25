//! Tests for `src/ipc/plugin/plugin_ipc.rs`, attached to it by `#[path]`.

use super::{CleanupScope, bump_capped, cleanup_scope_for_uninstall, commit_install};
use crate::plugins::name::PluginName;

/// The plugin-driven byte cap holds exactly at the boundary and cannot be wrapped past:
/// `usize::MAX + 2` would read as 1, well under the cap, if the sum did not saturate.
#[test]
fn bump_capped_stops_at_the_cap_and_never_wraps() {
    for (running, add, max, expected) in [
        (0, 100, 1000, Some(100)),
        (100, 50, 1000, Some(150)),
        (60, 4, 64, Some(64)),
        (60, 5, 64, None),
        (usize::MAX, 2, 1024, None),
    ] {
        let outcome = bump_capped(running, add, max);
        match expected {
            Some(total) => assert_eq!(outcome.unwrap(), total, "{running} + {add} under {max}"),
            None => {
                let err = outcome.unwrap_err();
                assert!(
                    err.to_string().contains("cap"),
                    "{running} + {add} under {max}: got {err}"
                );
            }
        }
    }
}

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

/// Replying Ok to an install whose trust reset failed kept the old decisions, which answer again
/// for identical bytes. The install fails instead, and the previous one stays as it was.
#[test]
fn a_failed_trust_reset_fails_the_install_and_keeps_the_previous_one() {
    let mut pc = rusqlite::Connection::open_in_memory().unwrap();
    crate::plugins::store::init_schema(&mut pc).unwrap();
    let name = PluginName::parse("FailedReset").unwrap();
    crate::plugins::store::install(&pc, "https://x/p", &name, "{}", "old", "h1").unwrap();
    // No `native_trust` table: the reset's DELETE fails.
    let mut sc = rusqlite::Connection::open_in_memory().unwrap();

    let outcome = commit_install(&pc, &mut sc, "https://x/p", &name, "{}", "new", "h2");

    assert!(outcome.is_err());
    let code: String = pc
        .query_row(
            "SELECT code FROM plugins WHERE url = ?1",
            ["https://x/p"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(code, "old");
}
