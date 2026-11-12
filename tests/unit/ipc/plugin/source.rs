//! Tests for `src/ipc/plugin/source.rs`, attached to it by `#[path]`.

use super::{bump_capped, serves_stored_code};

/// A wrong yes leaves a plugin on old code without any error.
/// `M2L3YHHyO7k` and `zc/AUeTSimY` are Bun's `xxHash64` of the two codes, little-endian, in
/// base64 without padding, the form esbuild writes.
#[test]
fn stored_code_is_reused_only_while_its_manifest_hash_still_names_it() {
    let (old, new) = ("export const x = 1;\n", "export const x = 2;\n");
    let manifest = |hash: &str| format!(r#"{{"name":"p","hash":"{hash}"}}"#);
    for (stored_manifest, stored_code, fetched_manifest, reused, case) in [
        (
            manifest("zc/AUeTSimY"),
            new,
            manifest("zc/AUeTSimY"),
            true,
            "unchanged",
        ),
        (
            manifest("M2L3YHHyO7k"),
            old,
            manifest("zc/AUeTSimY"),
            false,
            "hash changed",
        ),
        (
            manifest("zc/AUeTSimY"),
            old,
            manifest("zc/AUeTSimY"),
            false,
            "stored mid-publish",
        ),
        (
            r#"{"name":"p"}"#.to_string(),
            new,
            r#"{"name":"p"}"#.to_string(),
            false,
            "no hash",
        ),
        (
            r#"{"hash":7}"#.to_string(),
            new,
            r#"{"hash":7}"#.to_string(),
            false,
            "not a string",
        ),
    ] {
        assert_eq!(
            serves_stored_code(&stored_manifest, stored_code, &fetched_manifest),
            reused,
            "{case}"
        );
    }
}

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
