//! Tests for `src/ipc/caller.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn require_plugin_refuses_an_unattributed_caller() {
    let refused = Caller::Unattributed.require_plugin().err();
    assert_eq!(
        refused.map(|(code, _)| code),
        Some(403),
        "an unattributed call must be refused"
    );
}
