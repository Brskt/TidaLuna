//! Tests for `src/ui/folder_picker/windows.rs`, attached to it by `#[path]`.

use super::wide_to_path;

/// A name NTFS allows but Unicode does not is refused, never turned into another folder's name.
#[test]
fn a_folder_name_that_is_not_unicode_is_refused() {
    let valid: Vec<u16> = "C:\\Music".encode_utf16().collect();
    assert_eq!(wide_to_path(&valid).as_deref(), Some("C:\\Music"));
    assert_eq!(wide_to_path(&[u16::from(b'C'), 0xD800]), None);
}
