//! Tests for `src/ipc/plugin/plugin_ipc.rs`, attached to it by `#[path]`.

use super::{CleanupScope, bump_capped, cleanup_scope_for_uninstall, manifest_absent};

#[test]
fn bump_capped_accumulates_under_cap() {
    assert_eq!(bump_capped(0, 100, 1000).unwrap(), 100);
    assert_eq!(bump_capped(100, 50, 1000).unwrap(), 150);
}

#[test]
fn bump_capped_allows_exactly_at_cap() {
    assert_eq!(bump_capped(60, 4, 64).unwrap(), 64);
}

#[test]
fn bump_capped_rejects_over_cap() {
    let err = bump_capped(60, 5, 64).unwrap_err();
    assert!(err.to_string().contains("cap"), "got: {err}");
}

#[test]
fn bump_capped_saturates_on_huge_add() {
    // A pathological chunk length must not overflow the running total into a pass.
    let err = bump_capped(usize::MAX - 1, usize::MAX, 1024).unwrap_err();
    assert!(err.to_string().contains("cap"), "got: {err}");
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
