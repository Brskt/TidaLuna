//! Tests for `src/ipc/plugin/admission.rs`, attached by `#[path]`.

use super::*;

const fn at(epoch: u64, page: u64) -> Ticket {
    Ticket {
        epoch,
        page,
    }
}

const NOW: Ticket = at(3, 5);

/// No plugin line may run in a page or session the work was not started for, nor without a
/// session the app knows.
#[test]
fn plugin_code_is_admitted_only_in_a_known_session_and_for_the_current_page() {
    for (session_known, asked, expected, why) in [
        (
            true,
            at(3, 4),
            Err(Closed::Stale),
            "a pass of the page before a reload would run in the new one before its pins",
        ),
        (
            true,
            at(2, 5),
            Err(Closed::Stale),
            "a pass of the session before a logout would run in the one after",
        ),
        (true, NOW, Ok(()), "the current page of a known session"),
        (
            false,
            NOW,
            Err(Closed::NoSession),
            "a page whose token the app does not know may hold a real one a plugin could read",
        ),
        (
            false,
            at(2, 5),
            Err(Closed::Stale),
            "work from before a logout is dropped quietly, not announced as a missing session",
        ),
    ] {
        assert_eq!(
            entry(NOW, asked, standing(session_known)),
            expected,
            "{why}"
        );
    }
}
