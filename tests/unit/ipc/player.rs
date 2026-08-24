//! Tests for `src/ipc/player.rs`, attached to it by `#[path]`.

use super::{same_track, should_redact_args};

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

#[test]
fn the_measured_length_travels_with_its_own_track() {
    // One id, one track: a frame repeated for the track still playing keeps its length.
    assert!(same_track(Some("88264189"), Some("88264189")));
}

#[test]
fn a_length_is_never_lent_to_another_track() {
    // The tag is the measurement's own: it still names the track it was taken on however
    // many frames for the next track have gone by. Comparing against the shared metadata
    // slot instead let a track's second frame claim its predecessor's length.
    assert!(!same_track(Some("120002099"), Some("88264189")));
}

#[test]
fn an_unidentified_payload_is_never_a_match() {
    // Two anonymous payloads are not evidence of sameness: the OS controls are better
    // told no length than the previous track's.
    assert!(!same_track(None, None));
    assert!(!same_track(Some("1"), None));
    assert!(!same_track(None, Some("1")));
}
