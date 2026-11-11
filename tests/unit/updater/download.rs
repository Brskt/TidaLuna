//! Tests for `src/updater/download.rs`, attached to it by `#[path]`.

use super::bump_capped;

#[test]
fn bump_capped_accepts_up_to_the_cap_and_rejects_beyond_it() {
    for (used, add, cap, total) in [
        (0, 100, 1000, Some(100)),
        (100, 50, 1000, Some(150)),
        (60, 4, 64, Some(64)),
        (60, 5, 64, None),
    ] {
        let result = bump_capped(used, add, cap);
        match total {
            Some(total) => assert_eq!(result.unwrap(), total, "{used} + {add} under {cap}"),
            None => {
                let err = result.unwrap_err();
                assert!(err.to_string().contains("cap"), "got: {err}");
            }
        }
    }
}
