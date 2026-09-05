//! Tests for the plugin-load single-flight rule in `src/ipc/plugin/jsrt.rs`, attached by `#[path]`.
//!
//! One pass runs at a time and answers only the requests made under its own session epoch. A
//! logout bumps that epoch; a pass still running for the session the user left can no longer
//! report it as loaded to whoever asked afterwards.

use super::*;

#[test]
fn a_pass_answers_the_waiters_of_its_own_epoch() {
    let parked = vec![(7, 'a'), (7, 'b')];

    let (answer, rest, next) = settle_split(parked, 7);

    assert_eq!(answer, vec![(7, 'a'), (7, 'b')]);
    assert!(rest.is_empty());
    assert_eq!(next, None, "nothing is left to run a second pass for");
}

#[test]
fn a_waiter_from_a_newer_session_is_not_answered_by_the_older_pass() {
    // The logout landed between the two requests. The second belongs to another session.
    let parked = vec![(7, 'a'), (8, 'b')];

    let (answer, rest, next) = settle_split(parked, 7);

    assert_eq!(answer, vec![(7, 'a')]);
    assert_eq!(rest, vec![(8, 'b')]);
    assert_eq!(next, Some(8), "the newer session is owed a pass of its own");
}

#[test]
fn a_stale_pass_answers_nobody_and_still_hands_over() {
    // Every waiter arrived after the session changed: this pass describes a session none of them
    // asked about, and must hand the queue on rather than reply from it.
    let parked = vec![(9, 'a'), (9, 'b')];

    let (answer, rest, next) = settle_split(parked, 7);

    assert!(answer.is_empty());
    assert_eq!(rest, vec![(9, 'a'), (9, 'b')]);
    assert_eq!(next, Some(9));
}

#[test]
fn an_empty_queue_starts_nothing() {
    let (answer, rest, next) = settle_split(Vec::<(u64, char)>::new(), 7);

    assert!(answer.is_empty());
    assert!(rest.is_empty());
    assert_eq!(next, None);
}

#[test]
fn waiters_keep_their_arrival_order_within_an_epoch() {
    // The queue is answered in the order it was parked; `partition` is stable and the reply loop
    // walks it as-is.
    let parked = vec![(7, 'a'), (8, 'x'), (7, 'b'), (8, 'y')];

    let (answer, rest, next) = settle_split(parked, 7);

    assert_eq!(answer, vec![(7, 'a'), (7, 'b')]);
    assert_eq!(rest, vec![(8, 'x'), (8, 'y')]);
    assert_eq!(next, Some(8));
}
