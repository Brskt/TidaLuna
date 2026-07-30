//! Tests for `src/ui/file_dialog.rs`, attached to it by `#[path]`.

use super::*;

/// The regression this guards is a dismissed dialog reporting no selection at all, which
/// `showSaveDialog` then reported to the plugin as `canceled`.
#[test]
fn a_borrowed_list_is_read_through_the_raw_pointer() {
    let mut owned = CefStringList::new();
    owned.append("/music/song.flac");

    // The same shape CEF hands the callback: a borrow of a list it still owns.
    let raw: *mut sys::_cef_string_list_t = (&mut owned).into();
    let mut borrowed = CefStringList::from(raw);

    assert_eq!(
        read_paths(&mut borrowed),
        vec!["/music/song.flac".to_string()]
    );
}
