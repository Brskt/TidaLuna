//! Tests for `src/ipc/plugin/inject.rs`, attached to it by `#[path]`.

use super::*;

/// A logout landing while a plugin is transpiled leaves its injection untracked in a session nobody
/// is in. The epoch is re-read once the injection is out. An unreadable state (`None`) answers
/// like a change: an absence is never consent.
#[test]
fn an_injection_is_recorded_only_while_its_session_is_provably_current() {
    for (current, expected, why) in [
        (
            Some(7),
            true,
            "the ordinary case: nothing changed while this plugin was being prepared",
        ),
        (
            Some(8),
            false,
            "a logout landed mid-transpile and this code now runs for a session nobody is in",
        ),
        (
            None,
            false,
            "the state is unreadable, not an epoch that matches",
        ),
    ] {
        assert_eq!(injection_still_current(7, current), expected, "{why}");
    }
}

/// The boot pass sorts an outcome into what it owes. A logout under an injection owes nothing:
/// reconciled as the plugin's own failure it wrote `enabled = 0` behind a log level that is off by
/// default. Outcomes that end under their own session still owe their url.
#[test]
fn an_outcome_owes_the_boot_pass_a_verdict_only_under_its_own_session() {
    let duty = |outcome: InjectFailure| match outcome.pass_duty() {
        PassDuty::Reconcile => "reconcile".to_string(),
        PassDuty::Abandon => "abandon".to_string(),
        PassDuty::Keep => "keep".to_string(),
    };

    for (outcome, expected, why) in [
        (
            InjectFailure::SessionChanged,
            "abandon",
            "a logout under an injection would be reconciled as the plugin's own failure",
        ),
        (
            InjectFailure::RngUnavailable,
            "reconcile",
            "no nonce could be minted: nothing was dispatched and the url is still owed",
        ),
        (
            InjectFailure::TranspileFailed("unterminated string literal".to_string()),
            "reconcile",
            "the plugin's own source is what failed to prepare here",
        ),
        (
            InjectFailure::HelperUnavailable("cannot start the transpile helper".to_string()),
            "keep",
            "a helper that cannot run fails every plugin of the pass, and none of them is at fault",
        ),
        (
            InjectFailure::NoFrame,
            "reconcile",
            "no frame took the code, and the session is still the one that asked for it",
        ),
    ] {
        assert_eq!(duty(outcome), expected, "{why}");
    }
}
