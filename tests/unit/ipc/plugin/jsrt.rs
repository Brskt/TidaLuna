//! Tests for the plugin-load single-flight rule in `src/ipc/plugin/jsrt.rs`, attached by `#[path]`.
//!
//! One pass runs at a time and answers only the requests made under its own session epoch. A
//! logout bumps that epoch; a pass still running for the session the user left can no longer
//! report it as loaded to whoever asked afterwards.

use super::*;

/// The queue is answered in arrival order (`partition` is stable) and only for the pass's own
/// epoch. A waiter of another session is handed on, owed a pass of its own, rather than answered
/// from a pass that describes a session it never asked about.
#[test]
fn a_pass_answers_only_the_waiters_of_its_own_epoch_in_arrival_order() {
    for (what, parked, answer, rest, next) in [
        (
            "every waiter is of the pass's epoch",
            vec![(7, 'a'), (7, 'b')],
            vec![(7, 'a'), (7, 'b')],
            vec![],
            None,
        ),
        (
            "a logout landed between requests",
            vec![(7, 'a'), (8, 'x'), (7, 'b'), (8, 'y')],
            vec![(7, 'a'), (7, 'b')],
            vec![(8, 'x'), (8, 'y')],
            Some(8),
        ),
        (
            "every waiter arrived after the session changed",
            vec![(9, 'a'), (9, 'b')],
            vec![],
            vec![(9, 'a'), (9, 'b')],
            Some(9),
        ),
    ] {
        let (got_answer, got_rest, got_next) = settle_split(parked, 7);

        assert_eq!(got_answer, answer, "{what}");
        assert_eq!(got_rest, rest, "{what}");
        assert_eq!(got_next, next, "{what}");
    }
}
