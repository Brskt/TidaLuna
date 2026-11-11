//! Tests for `src/ui/folder_picker/linux.rs`, attached to it by `#[path]`.

use super::{portal_left, request_path, uri_to_path};

/// The portal's answer becomes a folder plugins may write into: only one plain `file://` folder,
/// decoded exactly, gets through.
#[test]
fn only_one_exact_file_uri_becomes_a_folder() {
    assert_eq!(
        uri_to_path(&["file:///home/me/My%20Music%25"]).as_deref(),
        Some("/home/me/My Music%")
    );
    // Not UTF-8: refused rather than turned into another folder's name.
    assert_eq!(uri_to_path(&["file:///home/me/%FF"]), None);
    assert_eq!(uri_to_path(&["https://example.org/home"]), None);
    assert_eq!(uri_to_path(&["file:///a", "file:///b"]), None);
    assert_eq!(uri_to_path(&[]), None);
}

/// The answer is awaited on this path; a different one would leave the picker unanswered forever.
#[test]
fn the_request_path_follows_the_portal_rule() {
    assert_eq!(
        request_path(":1.42", "tidalunar_0"),
        "/org/freedesktop/portal/desktop/request/1_42/tidalunar_0"
    );
}

/// Only the portal process exiting ends the wait: a false exit would refuse a dialog still on
/// screen, a missed one would hold every later picker forever.
#[test]
fn only_the_portal_process_exiting_ends_the_wait() {
    assert!(portal_left(":1.2", ":1.2", "", ":1.2"));
    // The well-known name changing hands: the process answering us may live on.
    assert!(!portal_left(
        "org.freedesktop.portal.Desktop",
        ":1.2",
        "",
        ":1.2"
    ));
    assert!(!portal_left(":1.9", ":1.9", "", ":1.2"), "another client");
    assert!(!portal_left(":1.2", "", ":1.2", ":1.2"), "a name arriving");
}
