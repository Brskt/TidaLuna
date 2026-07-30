//! Tests for `src/ipc/player.rs`, attached to it by `#[path]`.

use super::should_redact_args;

#[test]
fn redacts_known_secret_channels() {
    assert!(should_redact_args("jsrt.set_token"));
    assert!(should_redact_args("connect.controller.set_auth"));
    assert!(should_redact_args("player.load"));
    assert!(should_redact_args("player.load_dash"));
}

#[test]
fn redacts_privileged_channels_by_default() {
    // A privileged channel not in any explicit list must still be redacted, so
    // a secret-bearing one added later can't leak its args by omission.
    assert!(should_redact_args("jsrt.session_clear"));
    assert!(should_redact_args("settings.set_log_level"));
    assert!(should_redact_args("updater.apply"));
}

#[test]
fn keeps_benign_channels_visible() {
    assert!(!should_redact_args("player.play"));
    assert!(!should_redact_args("web.loaded"));
    assert!(!should_redact_args("menu.clicked"));
}
