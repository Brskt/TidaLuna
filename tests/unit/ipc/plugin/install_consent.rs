//! Tests for `src/ipc/plugin/install_consent.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn only_an_install_click_lets_the_install_through() {
    let consents = Consents::default();
    assert_eq!(consents.begin().unwrap().settle(Some(true)), Ok(()));
    assert_eq!(
        consents.begin().unwrap().settle(Some(false)),
        Err(Refusal::Declined)
    );
    assert_eq!(
        consents.begin().unwrap().settle(None),
        Err(Refusal::Unanswered)
    );
}

#[test]
fn a_second_install_is_refused_while_the_first_waits() {
    let consents = Consents::default();
    let first = consents.begin().unwrap();
    assert_eq!(consents.begin().err(), Some(Refusal::Busy));

    // A turn dropped without an answer (a failed fetch) frees the dialog too.
    drop(first);
    assert!(consents.begin().is_ok());
}
