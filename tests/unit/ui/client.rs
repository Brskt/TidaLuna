//! Tests for `src/ui/client.rs`, attached to it by `#[path]`.

use super::{
    DocumentEnd, PrivilegedGate, is_privileged_channel, privileged_gate, trusted_via_memo,
};
use crate::ui::nav::PageKind;

/// The memo follows the direct classification for every url, through a hit and a thrash.
#[test]
fn trust_memo_keeps_the_direct_classification_across_hits_and_thrash() {
    let mut memo = None;
    for (url, trusted) in [
        ("https://desktop.tidal.com/", true),
        ("https://desktop.tidal.com/login", true),
        ("https://desktop.tidal.com/login/auth", true),
        ("https://login.tidal.com/authorize", true),
        ("tidal://login/auth", true),
        ("https://evil.example.com/", false),
        ("not a url", false),
        ("https://desktop.tidal.com/", true),
        ("https://desktop.tidal.com/", true),
        ("https://evil.example.com/", false),
        ("https://desktop.tidal.com/", true),
        ("https://evil.example.com/", false),
    ] {
        let direct = !matches!(PageKind::classify(url), PageKind::External);
        let verdict = trusted_via_memo(&mut memo, url.to_string());
        assert_eq!(verdict, direct, "memoized verdict diverged for {url}");
        assert_eq!(verdict, trusted, "{url}");
    }
}

/// A privileged channel called with an id (invoke) from an untrusted frame gets a 403, not a
/// silent ok, whatever the dispatch list says. With no id there is no JS consumer: drop with an ack.
#[test]
fn an_untrusted_frame_is_refused_a_privileged_channel_and_nothing_else() {
    for (privileged, trusted, has_id, expected) in [
        (true, false, true, PrivilegedGate::Refuse403),
        (true, false, false, PrivilegedGate::DropAck),
        (true, true, true, PrivilegedGate::Allow),
        (true, true, false, PrivilegedGate::Allow),
        (false, false, true, PrivilegedGate::Allow),
        (false, false, false, PrivilegedGate::Allow),
    ] {
        assert_eq!(
            privileged_gate(privileged, trusted, has_id),
            expected,
            "privileged: {privileged}, trusted: {trusted}, has id: {has_id}"
        );
    }
}

/// `parse_dash` (frame-gated in cefQuery; the frame-less console bridge must drop it too), the
/// token, `window.navigate_self`, `window.open_url` and `player.devices.*` (which switch the OS
/// output, `set` can ask for exclusive mode) are refused; playback and page chrome stay open.
#[test]
fn only_the_channels_that_reach_the_token_the_os_or_the_dash_parser_are_privileged() {
    for (channel, privileged) in [
        ("player.parse_dash", true),
        ("player.play", false),
        ("player.load_dash", false),
        ("jsrt.set_token", true),
        ("window.navigate_self", true),
        ("window.minimize", false),
        ("window.open_url", true),
        ("player.devices.get", true),
        ("player.devices.set", true),
    ] {
        assert_eq!(is_privileged_channel(channel), privileged, "{channel}");
    }
}

#[test]
fn only_the_app_browsers_main_frame_ends_the_page_that_plays() {
    let table = [
        (true, true, DocumentEnd::Owner),
        (true, false, DocumentEnd::Other),
        (false, true, DocumentEnd::Other),
        (false, false, DocumentEnd::Other),
    ];
    for (in_app_browser, is_main_frame, expected) in table {
        assert_eq!(
            DocumentEnd::of(in_app_browser, is_main_frame),
            expected,
            "app browser: {in_app_browser}, main frame: {is_main_frame}"
        );
    }
}
