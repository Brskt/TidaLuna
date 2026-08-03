//! Tests for `src/ipc/caller.rs`, attached to it by `#[path]`.

use super::*;
use crate::app_state::IpcMessage;

fn envelope(json: &str) -> IpcMessage {
    serde_json::from_str(json).expect("envelope parses")
}

/// The app bundle's senders never send `cap`; their envelopes must keep parsing.
#[test]
fn an_envelope_without_a_cap_field_still_parses() {
    let msg = envelope(r#"{"channel":"player.parse_dash","args":["x"]}"#);
    assert_eq!(msg.channel, "player.parse_dash");
    assert!(msg.cap.is_none());
}

#[test]
fn a_cap_field_is_read_off_the_envelope_not_the_arguments() {
    let msg = envelope(r#"{"channel":"plugin.download","args":["payload"],"cap":"abc123"}"#);
    assert_eq!(msg.cap.as_deref(), Some("abc123"));
    // The reserved field must not disturb positional arguments: every existing handler indexes
    // from zero and would silently read the wrong value if the capability took a slot.
    assert_eq!(msg.arg(0), "payload");
}

#[test]
fn a_message_without_a_capability_is_unattributed() {
    let msg = envelope(r#"{"channel":"plugin.download","args":[]}"#);
    assert!(matches!(Caller::resolve(&msg), Caller::Unattributed));
}

/// The positive plugin path needs a live `AppState`: it lives in the `PluginManager` capability
/// tests instead.
#[test]
fn a_capability_no_one_claims_is_unattributed() {
    let msg = envelope(r#"{"channel":"plugin.download","args":[],"cap":"forged"}"#);
    assert!(matches!(Caller::resolve(&msg), Caller::Unattributed));
}

#[test]
fn require_plugin_refuses_an_unattributed_caller() {
    let (code, _) = Caller::Unattributed
        .require_plugin()
        .expect_err("an unattributed call must be refused");
    assert_eq!(code, 403);
}

#[test]
fn require_plugin_yields_the_plugin_url() {
    let caller = Caller::Plugin("https://example.test/a.js".to_string());
    assert_eq!(
        caller.require_plugin().expect("attributed"),
        "https://example.test/a.js"
    );
}
