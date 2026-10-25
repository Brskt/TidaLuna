//! Tests for `src/ipc/player.rs`, attached to it by `#[path]`.

use super::{same_track, should_redact_args};

/// A privileged channel in no explicit list must still be redacted; a secret-bearing one added
/// later would otherwise leak its args by omission.
#[test]
fn secret_and_privileged_channels_are_redacted() {
    for channel in [
        "jsrt.set_token",
        "connect.controller.set_auth",
        "player.load",
        "player.load_dash",
        "jsrt.session_clear",
        "settings.set_log_level",
        "updater.apply",
    ] {
        assert!(should_redact_args(channel), "{channel} must be redacted");
    }
}

/// A measured length belongs to the track it was taken on, however many frames for the next one
/// have gone by; two anonymous payloads are no evidence of sameness, and the OS controls are
/// better told no length than the previous track's.
#[test]
fn a_measured_length_travels_only_with_its_own_identified_track() {
    for (measured, announced, expected) in [
        (Some("88264189"), Some("88264189"), true),
        (Some("120002099"), Some("88264189"), false),
        (None, None, false),
        (Some("1"), None, false),
        (None, Some("1"), false),
    ] {
        assert_eq!(
            same_track(measured, announced),
            expected,
            "{measured:?} against {announced:?}"
        );
    }
}
