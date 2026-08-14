//! Tests for `src/updater/util.rs`, attached to it by `#[path]`.

use super::{is_newer, meets_min_version};

#[test]
fn is_newer_orders_patch_numerically() {
    assert!(is_newer("0.0.10-alpha", "0.0.9-alpha"));
    assert!(!is_newer("0.0.9-alpha", "0.0.10-alpha"));
}

#[test]
fn is_newer_release_beats_its_prerelease() {
    // SemVer 2.0: 0.0.9 > 0.0.9-alpha
    assert!(is_newer("0.0.9", "0.0.9-alpha"));
    assert!(!is_newer("0.0.9-alpha", "0.0.9"));
}

#[test]
fn is_newer_orders_prerelease_identifiers() {
    // -alpha.10 > -alpha.2 (numeric identifier compare, not string)
    assert!(is_newer("0.0.9-alpha.10", "0.0.9-alpha.2"));
    assert!(!is_newer("0.0.9-alpha.2", "0.0.9-alpha.10"));
}

#[test]
fn is_newer_equal_is_not_newer() {
    assert!(!is_newer("0.0.9-alpha", "0.0.9-alpha"));
}

#[test]
fn is_newer_unparseable_is_failsafe_false() {
    assert!(!is_newer("garbage", "0.0.9-alpha"));
    assert!(!is_newer("0.0.9-alpha", "not-a-version"));
}

#[test]
fn meets_min_version_floor_satisfied() {
    assert!(meets_min_version("0.0.9-alpha", "0.0.0")); // no-op floor
    assert!(meets_min_version("0.0.9-alpha", "0.0.8-alpha"));
    assert!(meets_min_version("0.0.8-alpha", "0.0.8-alpha")); // equal meets floor
}

#[test]
fn meets_min_version_below_floor_blocked() {
    assert!(!meets_min_version("0.0.7-alpha", "0.0.8-alpha"));
}

#[test]
fn meets_min_version_unparseable_is_failclosed() {
    assert!(!meets_min_version("0.0.9-alpha", "")); // empty floor -> block
    assert!(!meets_min_version("0.0.9-alpha", "garbage"));
}

#[test]
fn promoted_release_beats_its_dev_builds() {
    // Raw SemVer says alpha.dev.5 > alpha; the dev-aware key inverts that.
    assert!(is_newer("0.0.14-alpha", "0.0.14-alpha.dev.5"));
    assert!(!is_newer("0.0.14-alpha.dev.5", "0.0.14-alpha"));
    // Same rule in the bare-release phase.
    assert!(is_newer("0.1.0", "0.1.0-dev.3"));
    assert!(!is_newer("0.1.0-dev.3", "0.1.0"));
}

#[test]
fn dev_counters_compare_numerically() {
    assert!(is_newer("0.0.14-alpha.dev.10", "0.0.14-alpha.dev.9"));
    assert!(!is_newer("0.0.14-alpha.dev.9", "0.0.14-alpha.dev.10"));
}

#[test]
fn dev_builds_of_a_newer_base_beat_older_releases() {
    assert!(is_newer("0.0.14-alpha.dev.1", "0.0.13-alpha"));
    assert!(is_newer("0.0.20-beta", "0.0.20-alpha.dev.7")); // phase change wins
    assert!(!is_newer("0.0.13-alpha", "0.0.14-alpha.dev.1"));
}

#[test]
fn non_dev_prerelease_lists_keep_semver_order() {
    // A trailing identifier pair that is not `dev.N` stays raw SemVer.
    assert!(is_newer("0.0.9-alpha.rc.2", "0.0.9-alpha"));
}

#[test]
fn meets_min_version_is_dev_aware() {
    // A dev build sits below its promoted base; it misses that floor.
    assert!(!meets_min_version("0.0.14-alpha.dev.5", "0.0.14-alpha"));
    assert!(meets_min_version("0.0.14-alpha", "0.0.14-alpha.dev.5"));
}
