//! Tests for `src/ui/client/load.rs`, attached to it by `#[path]`.

use super::DocumentEnd;

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
