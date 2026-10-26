//! Tests for `src/ipc/plugin/update.rs`, attached to it by `#[path]`.

use super::*;

/// A `.json` missing for a moment must not replace a real manifest (its dependencies, its type)
/// with the one made up for a plugin that never had one; a real `.json` replaces it whatever
/// keys it keeps.
#[test]
fn only_a_made_up_manifest_is_kept_from_a_real_one() {
    let real = r#"{"name":"p","main":"p.mjs","luna":{"type":"library"}}"#;
    let made_up = r#"{"main":"https://a/p.mjs","name":"p"}"#;
    let cases = [
        (Manifest::MadeUp(made_up.to_string()), real, false),
        (Manifest::MadeUp(made_up.to_string()), made_up, true),
        (
            Manifest::Served(r#"{"name":"p","main":"p.mjs"}"#.to_string()),
            real,
            true,
        ),
    ];
    for (fetched, stored, replaces) in cases {
        assert_eq!(fetched.replaces(stored), replaces, "{stored}");
    }
}
