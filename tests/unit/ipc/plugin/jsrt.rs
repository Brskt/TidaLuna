//! Tests for `src/ipc/plugin/jsrt/mod.rs`, attached by `#[path]`.

use super::*;

/// When this machine could not prepare a plugin, the plugins stranded behind it are told and keep
/// their flag: a disable would outlive the glitch. A plugin whose dependency failed by itself is
/// turned off.
#[test]
fn a_machine_failure_tells_the_plugins_it_stranded_instead_of_disabling_them() {
    let waiting: HashMap<String, String> = [("https://x/W".to_string(), "W".to_string())].into();
    let unresolved = || vec!["https://x/D".to_string(), "https://x/W".to_string()];

    assert_eq!(
        settle_unresolved(unresolved(), &waiting, true),
        Unresolved {
            failed: vec![],
            stopped: vec!["https://x/D".to_string()],
        },
        "a machine failure disabled a plugin, or a plugin waiting on a store move was told"
    );
    assert_eq!(
        settle_unresolved(unresolved(), &waiting, false),
        Unresolved {
            failed: vec!["https://x/D".to_string()],
            stopped: vec![],
        },
        "a plugin whose dependency failed was left on"
    );
}
