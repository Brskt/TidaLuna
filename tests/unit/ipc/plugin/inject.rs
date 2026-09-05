//! Tests for `src/ipc/plugin/inject.rs`, attached to it by `#[path]`.

use super::*;

/// A caller reads the epoch once, then transpiles, which is real CPU time holding no lock. A
/// logout landing in that gap sweeps the manager, and its cleanup for this plugin does
/// nothing, because `mark_loading` registered it before any code existed. The injection then
/// lands in a session the user has left, untracked and unreachable by any later unload, until
/// the next full page navigation. The answer has to be re-read once the injection is out,
/// not only before it.
#[test]
fn an_injection_that_outlived_its_session_is_not_recorded() {
    assert!(
        injection_still_current(7, Some(7)),
        "the ordinary case: nothing changed while this plugin was being prepared"
    );
    assert!(
        !injection_still_current(7, Some(8)),
        "a logout landed mid-transpile and this code now runs for a session nobody is in"
    );
}

/// `None` is the state being unreadable rather than an epoch that matches. Reading an absence
/// as consent is exactly how an injection outlives its session: it answers like a change.
#[test]
fn an_unreadable_state_refuses_the_injection_rather_than_assuming_it_is_fine() {
    assert!(!injection_still_current(7, None));
}

/// The boot pass reads an outcome to decide what it owes, and it used to do that through a
/// wildcard; `SessionChanged` reached reconciliation dressed as a plugin that could not
/// load. Reconciliation writes `enabled = 0` and leaves only a log line behind a level whose
/// persisted default is off, which turned a logout landing on the last plugin of a pass into a
/// plugin silently disabled for every session after it.
#[test]
fn a_session_that_ended_owes_no_verdict_on_the_plugin() {
    assert!(
        matches!(Injected::SessionChanged.pass_duty(), PassDuty::Abandon),
        "a logout under an injection would be reconciled as the plugin's own failure"
    );
}

/// The other side of the same rule. Three outcomes end with nothing live while the session that
/// asked for them is still the current one, and those do owe the reconciliation their url;
/// sorting the cancellation out must not quietly take them along.
#[test]
fn an_attempt_that_ended_under_its_own_session_still_owes_its_url() {
    assert!(
        matches!(Injected::RngUnavailable.pass_duty(), PassDuty::Reconcile),
        "no nonce could be minted, so nothing was dispatched and the url is still owed"
    );
    assert!(
        matches!(
            Injected::TranspileFailed("unterminated string literal".to_string()).pass_duty(),
            PassDuty::Reconcile
        ),
        "the plugin's own source is what failed to prepare here"
    );
    assert!(
        matches!(Injected::NoFrame.pass_duty(), PassDuty::Reconcile),
        "no frame took the code, and the session is still the one that asked for it"
    );
    assert!(
        matches!(
            Injected::Live { load_id: 7 }.pass_duty(),
            PassDuty::Record { load_id: 7 }
        ),
        "the readiness ack and its timeout are keyed to this load, so it has to survive the sort"
    );
}
