//! Tests for `src/ipc/plugin/lib_native.rs`, attached to it by `#[path]`.

use super::{asked_for_a_directory, build_send_to_render_js, names_a_directory};
use serde_json::json;

#[test]
fn send_to_render_escapes_separators_and_encodes_channel() {
    // A string arg containing U+2028 (a JS line terminator) must be escaped so
    // it can't terminate the emit statement and inject the following code.
    let js = build_send_to_render_js("ch", &[json!("a\u{2028}b")]);
    assert!(js.contains("\\u2028"), "U+2028 must be escaped: {js}");
    assert!(!js.contains('\u{2028}'), "raw U+2028 must not remain: {js}");
    // The channel is a JSON string literal that can't break out.
    assert!(js.contains("__LUNAR_IPC_EMIT__(\"ch\","));
}

#[test]
fn send_to_render_no_args_omits_trailing_comma() {
    let js = build_send_to_render_js("ch", &[]);
    assert!(js.ends_with("__LUNAR_IPC_EMIT__(\"ch\");"), "{js}");
}

/// A bare directory as `defaultPath` is the documented Electron idiom for "open here, suggest no
/// name". Sanitising it treated the last component as a file name (`Path::file_name` ignores a
/// trailing separator): the dialog opened in the PARENT with the folder's name pre-filled.
#[test]
fn a_default_path_naming_a_directory_is_left_alone() {
    assert!(names_a_directory("/home/u/Music/"));

    let dir = tempfile::tempdir().unwrap();
    assert!(
        names_a_directory(&dir.path().to_string_lossy()),
        "an existing directory counts even without a trailing separator"
    );

    assert!(!names_a_directory("/home/u/Music/song.flac"));
    assert!(!names_a_directory(""));
}

/// The write grant follows the request for a directory, not the dialog in general: a plugin opening
/// a file dialog to read something must not gain write access to the folder it browsed.
#[test]
fn only_a_directory_request_grants_a_write_folder() {
    assert!(asked_for_a_directory(&["openDirectory"]));
    assert!(asked_for_a_directory(&["openDirectory", "createDirectory"]));

    assert!(!asked_for_a_directory(&[]));
    assert!(!asked_for_a_directory(&["multiSelections"]));
    assert!(!asked_for_a_directory(&["createDirectory"]));
}
