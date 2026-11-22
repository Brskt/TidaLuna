//! Tests for `src/ipc/plugin/activation.rs`, attached by `#[path]`.

use std::collections::HashSet;

use super::*;

/// A restart unloads a plugin without turning it off; only a dependant still running may hold
/// it: one switched on but stopped imports nothing, and blocking on it would leave Reload stuck.
#[test]
fn only_a_running_dependant_holds_its_dependency_loaded() {
    let live: HashSet<String> = ["Running".to_string()].into();
    let dependants = vec!["Running".to_string(), "Stopped".to_string()];

    assert_eq!(
        live_dependants(dependants, &live),
        vec!["Running".to_string()]
    );
}
