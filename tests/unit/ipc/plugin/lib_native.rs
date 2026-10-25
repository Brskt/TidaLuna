//! Tests for `src/ipc/plugin/lib_native.rs`, attached to it by `#[path]`.

use super::{
    asked_for_a_directory, build_send_to_render_js, record_save_answer, vet_folder_answer,
    vet_save_answer,
};
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

/// The download looks its destination up sanitised. Recorded raw, the answer matched nothing and
/// the download was refused for want of a dialog whenever the title held a character Windows refuses.
#[test]
fn a_save_answer_is_recorded_under_its_sanitised_name() {
    use crate::ui::file_dialog::{authorisation_for, consume};

    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("Title: B?.flac");
    let sanitised = dir.path().join("Title_ B_.flac");

    record_save_answer(&raw.to_string_lossy()).unwrap();

    let auth = authorisation_for(&sanitised).expect("the sanitised name is authorised");
    consume(auth);
    assert!(
        authorisation_for(&raw).is_none(),
        "the raw answer is not a name the download will write"
    );
}

/// The guard judges the name the download writes. Windows strips the trailing dot: `install.`
/// is written as the install folder itself, which the raw spelling does not name on Linux.
#[test]
fn a_save_answer_is_judged_under_its_sanitised_name() {
    use crate::platform::install_guard::Protected;
    use crate::ui::file_dialog::NotRecorded;

    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("install");
    std::fs::create_dir(&root).unwrap();
    let guard = Protected::new(&root, &parent.path().join("data/native"));

    assert_eq!(
        vet_save_answer(Some(&guard), &parent.path().join("install.")),
        Err(NotRecorded::Protected)
    );
    assert_eq!(
        vet_save_answer(Some(&guard), &parent.path().join("Title: B?.flac")),
        Ok(())
    );
}

/// The install folder is refused as a folder, a folder beside it is not.
#[test]
fn a_protected_folder_answer_is_still_refused() {
    use crate::platform::install_guard::Protected;
    use crate::ui::file_dialog::NotRecorded;

    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("install");
    std::fs::create_dir(&root).unwrap();
    let guard = Protected::new(&root, &parent.path().join("data/native"));

    assert_eq!(
        vet_folder_answer(Some(&guard), &root),
        Err(NotRecorded::Protected)
    );
    let music = parent.path().join("Music");
    std::fs::create_dir(&music).unwrap();
    assert_eq!(vet_folder_answer(Some(&guard), &music), Ok(()));
}
