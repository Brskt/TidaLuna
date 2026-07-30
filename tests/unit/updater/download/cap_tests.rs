//! Tests for `src/updater/download.rs`, attached to it by `#[path]`.

use super::bump_capped;

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
