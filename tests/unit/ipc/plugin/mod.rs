//! Tests for `src/ipc/plugin/mod.rs`, attached to it by `#[path]`.

use super::*;

/// Each is reachable from the wrapper's own shims, which carry the capability: requiring it costs
/// no plugin change.
#[test]
fn the_channels_a_plugin_must_be_known_on_are_attributed() {
    for channel in [
        "plugin.storage.get",
        "plugin.storage.set",
        "plugin.storage.del",
        "plugin.storage.keys",
        "plugin.fetch",
    ] {
        assert!(
            is_plugin_attributed(channel),
            "{channel} must require attribution"
        );
    }
}

/// `plugin.fetch_package` starts with `plugin.fetch`, and it is how the app installs a plugin,
/// long before any plugin holds a capability. A prefix match here would refuse every install.
#[test]
fn the_install_channel_is_not_caught_by_the_fetch_prefix() {
    assert!(!is_plugin_attributed("plugin.fetch_package"));
}

/// The renderer sees only the code. It has to show a listener "pick a higher quality" for one
/// of these and stay quiet for the other, so reporting both as 400 left the message
/// unreachable and matching on the prose would lose it at the first rewording.
#[test]
fn an_undecodable_profile_reports_a_different_code_than_a_broken_manifest() {
    let undecodable: anyhow::Error = crate::player::dash::UndecodableProfile {
        codec: "mp4a.40.5".into(),
    }
    .into();
    assert_eq!(parse_dash_failure_code(&undecodable), 415);

    let malformed = anyhow::anyhow!("MPD has no periods");
    assert_eq!(parse_dash_failure_code(&malformed), 400);
}

/// Widening this to channels no capability reaches would refuse calls the app itself makes.
#[test]
fn channels_no_handler_attributes_are_left_alone() {
    for channel in [
        "player.parse_dash",
        "tidal.fetch",
        "proxy.head",
        "proxy.fetch",
        "plugin.install",
        "plugin.enable",
        "plugin.list",
        "plugin.download",
        "__Luna.showSaveDialog",
        "__Luna.clipboardWriteText",
    ] {
        assert!(
            !is_plugin_attributed(channel),
            "{channel} must not require attribution"
        );
    }
}
