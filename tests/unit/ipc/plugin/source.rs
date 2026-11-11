//! Tests for `src/ipc/plugin/source.rs`, attached to it by `#[path]`.

use super::bump_capped;

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
