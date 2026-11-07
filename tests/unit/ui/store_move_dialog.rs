//! Tests for `src/ui/store_move_dialog.rs`, attached to it by `#[path]`.

use super::*;

/// Only a button answers: any other navigation of the window is no answer, as a close is, and turns
/// no plugin off.
#[test]
fn only_the_two_buttons_answer_a_store_move() {
    assert_eq!(parse_move(MOVE_ALLOW), Some(Some(true)));
    assert_eq!(parse_move(MOVE_DENY), Some(Some(false)));
    assert_eq!(parse_move("about:blank"), None);
}
