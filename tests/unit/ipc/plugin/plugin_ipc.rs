//! Tests for `src/ipc/plugin/plugin_ipc.rs`, attached to it by `#[path]`.

use super::{CleanupScope, bump_capped, cleanup_scope_for_uninstall, manifest_absent};

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
fn an_uninstall_that_matched_no_plugin_clears_nothing() {
    assert!(cleanup_scope_for_uninstall(String::new()).is_none());
}

#[test]
fn an_uninstall_that_matched_a_plugin_clears_only_that_plugin() {
    match cleanup_scope_for_uninstall("DiscordRPC".to_string()) {
        Some(CleanupScope::Plugin(name)) => assert_eq!(name, "DiscordRPC"),
        _ => panic!("a matched plugin must scope the cleanup to itself"),
    }
}

#[test]
fn manifest_absent_only_for_404() {
    use reqwest::StatusCode;
    assert!(manifest_absent(StatusCode::NOT_FOUND));
    assert!(!manifest_absent(StatusCode::OK));
    // A server error is a real failure, not an absent manifest - must NOT fall back.
    assert!(!manifest_absent(StatusCode::INTERNAL_SERVER_ERROR));
    assert!(!manifest_absent(StatusCode::FORBIDDEN));
}
