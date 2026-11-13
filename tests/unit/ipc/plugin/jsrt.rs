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
        settle_unresolved(unresolved(), &waiting, &HashMap::new(), true),
        Unresolved {
            failed: vec![],
            stopped: vec!["https://x/D".to_string()],
            blocked: vec![],
        },
        "a machine failure disabled a plugin, or a plugin waiting on a store move was told"
    );
    assert_eq!(
        settle_unresolved(unresolved(), &waiting, &HashMap::new(), false),
        Unresolved {
            failed: vec!["https://x/D".to_string()],
            stopped: vec![],
            blocked: vec![],
        },
        "a plugin whose dependency failed was left on"
    );
}

/// A plugin behind one kept on without running, its stored data unreadable for one, keeps its flag
/// and is told, since a disable would outlast damage that is not its own.
#[test]
fn a_plugin_behind_one_kept_on_without_running_keeps_its_flag() {
    let blocked: HashMap<String, String> = [("https://x/B".to_string(), "B".to_string())].into();
    let unresolved = vec!["https://x/B".to_string(), "https://x/D".to_string()];

    assert_eq!(
        settle_unresolved(unresolved, &HashMap::new(), &blocked, false),
        Unresolved {
            failed: vec!["https://x/D".to_string()],
            stopped: vec![],
            blocked: vec!["https://x/B".to_string()],
        },
        "a plugin behind one not running was disabled"
    );
}
