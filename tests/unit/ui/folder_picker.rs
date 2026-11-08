//! Tests for `src/ui/folder_picker/mod.rs`, attached to it by `#[path]`.

use std::sync::atomic::AtomicBool;

use super::start_folder;
use crate::util::one_at_a_time::OneAtATime;

/// The plugin's `defaultPath` names where the picker opens: a folder as is, anything else by its
/// parent, and nothing usable not at all.
#[test]
fn the_picker_opens_on_the_folder_the_plugin_named() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().to_str().unwrap();
    let file = dir.path().join("track.flac");
    std::fs::write(&file, b"").unwrap();

    assert_eq!(start_folder(Some(folder)).as_deref(), Some(dir.path()));
    assert_eq!(start_folder(file.to_str()).as_deref(), Some(dir.path()));
    assert_eq!(
        start_folder(Some("track.flac")),
        None,
        "no folder to go up to"
    );
    assert_eq!(start_folder(Some("")), None);
    assert_eq!(start_folder(Some("/a\0b")), None);
    assert_eq!(start_folder(None), None);
}

/// Two plugins asking at once get one picker; the second can open once the first has answered.
#[test]
fn a_second_picker_waits_for_the_first_to_close() {
    static FLAG: AtomicBool = AtomicBool::new(false);
    let first = OneAtATime::try_acquire(&FLAG).expect("nothing open yet");
    assert!(OneAtATime::try_acquire(&FLAG).is_none());
    drop(first);
    assert!(OneAtATime::try_acquire(&FLAG).is_some());
}
