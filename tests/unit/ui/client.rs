//! Tests for `src/ui/client.rs`, attached to it by `#[path]`.

use super::{PrivilegedGate, is_privileged_channel, privileged_gate, trusted_via_memo};
use crate::ui::nav::PageKind;

#[test]
fn trust_memo_matches_direct_classification() {
    let urls = [
        "https://desktop.tidal.com/",
        "https://desktop.tidal.com/login",
        "https://desktop.tidal.com/login/auth",
        "https://login.tidal.com/authorize",
        "tidal://login/auth",
        "https://evil.example.com/",
        "not a url",
    ];
    let mut memo = None;
    for url in urls {
        let direct = !matches!(PageKind::classify(url), PageKind::External);
        assert_eq!(
            trusted_via_memo(&mut memo, url.to_string()),
            direct,
            "memoized verdict diverged for {url}"
        );
    }
}

#[test]
fn trust_memo_hit_and_thrash_keep_the_verdict() {
    let mut memo = None;
    let trusted = "https://desktop.tidal.com/";
    let external = "https://evil.example.com/";
    assert!(trusted_via_memo(&mut memo, trusted.to_string()));
    assert!(trusted_via_memo(&mut memo, trusted.to_string()));
    assert!(!trusted_via_memo(&mut memo, external.to_string()));
    assert!(trusted_via_memo(&mut memo, trusted.to_string()));
    assert!(!trusted_via_memo(&mut memo, external.to_string()));
}

#[test]
fn privileged_untrusted_req_resp_is_refused() {
    // A privileged channel called with an id (invoke) from an untrusted frame
    // must get a 403, not a silent "ok" - regardless of the dispatch list.
    assert_eq!(
        privileged_gate(true, false, true),
        PrivilegedGate::Refuse403
    );
}

#[test]
fn privileged_untrusted_fire_and_forget_is_dropped() {
    // No id = no JS consumer: drop with an ack rather than a 403.
    assert_eq!(privileged_gate(true, false, false), PrivilegedGate::DropAck);
}

#[test]
fn privileged_trusted_is_allowed() {
    assert_eq!(privileged_gate(true, true, true), PrivilegedGate::Allow);
    assert_eq!(privileged_gate(true, true, false), PrivilegedGate::Allow);
}

#[test]
fn benign_channel_is_always_allowed() {
    assert_eq!(privileged_gate(false, false, true), PrivilegedGate::Allow);
    assert_eq!(privileged_gate(false, false, false), PrivilegedGate::Allow);
}

#[test]
fn parse_dash_is_privileged() {
    // Dispatched by the plugin-IPC handler and frame-gated in cefQuery, so
    // the frame-less console bridge must drop it too.
    assert!(is_privileged_channel("player.parse_dash"));
}

#[test]
fn benign_player_controls_stay_open() {
    // Playback controls are fire-and-forget on the console bridge; only
    // parse_dash is gated, not the whole `player.*` namespace.
    assert!(!is_privileged_channel("player.play"));
    assert!(!is_privileged_channel("player.load_dash"));
}

#[test]
fn auth_and_window_self_are_privileged() {
    assert!(is_privileged_channel("jsrt.set_token"));
    assert!(is_privileged_channel("window.navigate_self"));
    assert!(!is_privileged_channel("window.minimize"));
}

#[test]
fn open_url_and_device_control_are_privileged() {
    // window.open_url pops an OS-browser URL; player.devices.* switches the OS
    // audio output (set can request exclusive mode). Both must be refused from
    // untrusted frames and dropped on the frame-less console bridge.
    assert!(is_privileged_channel("window.open_url"));
    assert!(is_privileged_channel("player.devices.get"));
    assert!(is_privileged_channel("player.devices.set"));
    // Benign page-chrome stays reachable.
    assert!(!is_privileged_channel("window.minimize"));
    assert!(!is_privileged_channel("player.play"));
}
