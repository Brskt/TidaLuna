//! Tests for `src/ipc/caller.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn require_plugin_refuses_an_unattributed_caller() {
    let (code, _) = Caller::Unattributed
        .require_plugin()
        .expect_err("an unattributed call must be refused");
    assert_eq!(code, 403);
}
