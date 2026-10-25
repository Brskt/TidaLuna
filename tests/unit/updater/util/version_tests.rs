//! Tests for `src/updater/util.rs`, attached to it by `#[path]`.

use super::{is_newer, meets_min_version};

#[test]
fn is_newer_orders_versions() {
    for (remote, current, newer) in [
        ("0.0.10-alpha", "0.0.9-alpha", true),
        ("0.0.9-alpha", "0.0.10-alpha", false),
        // SemVer 2.0: 0.0.9 > 0.0.9-alpha
        ("0.0.9", "0.0.9-alpha", true),
        ("0.0.9-alpha", "0.0.9", false),
        // Numeric identifier compare, not string.
        ("0.0.9-alpha.10", "0.0.9-alpha.2", true),
        ("0.0.9-alpha.2", "0.0.9-alpha.10", false),
        ("0.0.9-alpha", "0.0.9-alpha", false),
        // Unparseable fails safe.
        ("garbage", "0.0.9-alpha", false),
        ("0.0.9-alpha", "not-a-version", false),
        // Raw SemVer says alpha.dev.5 > alpha; the dev-aware key inverts that, bare releases too.
        ("0.0.14-alpha", "0.0.14-alpha.dev.5", true),
        ("0.0.14-alpha.dev.5", "0.0.14-alpha", false),
        ("0.1.0", "0.1.0-dev.3", true),
        ("0.1.0-dev.3", "0.1.0", false),
        ("0.0.14-alpha.dev.10", "0.0.14-alpha.dev.9", true),
        ("0.0.14-alpha.dev.9", "0.0.14-alpha.dev.10", false),
        ("0.0.14-alpha.dev.1", "0.0.13-alpha", true),
        // Phase change wins.
        ("0.0.20-beta", "0.0.20-alpha.dev.7", true),
        ("0.0.13-alpha", "0.0.14-alpha.dev.1", false),
        // A trailing identifier pair that is not `dev.N` stays raw SemVer.
        ("0.0.9-alpha.rc.2", "0.0.9-alpha", true),
    ] {
        assert_eq!(
            is_newer(remote, current),
            newer,
            "is_newer({remote:?}, {current:?})"
        );
    }
}

#[test]
fn meets_min_version_orders_versions_and_fails_closed() {
    for (installed, floor, meets) in [
        ("0.0.9-alpha", "0.0.0", true),
        ("0.0.9-alpha", "0.0.8-alpha", true),
        ("0.0.8-alpha", "0.0.8-alpha", true),
        // An empty or unparseable floor blocks.
        ("0.0.9-alpha", "", false),
        ("0.0.9-alpha", "garbage", false),
        // A dev build sits below its promoted base and misses that floor.
        ("0.0.14-alpha.dev.5", "0.0.14-alpha", false),
        ("0.0.14-alpha", "0.0.14-alpha.dev.5", true),
    ] {
        assert_eq!(
            meets_min_version(installed, floor),
            meets,
            "meets_min_version({installed:?}, {floor:?})"
        );
    }
}
