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
/// default. Nor does a reload, a quit or a machine's failure; the rest still owe their url.
#[test]
fn only_a_failed_draw_or_transpile_owes_the_boot_pass_a_verdict() {
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
            "abandon",
            "only a closing browser has no frame: a quit under an injection would disable the plugin",
        ),
        (
            InjectFailure::PageReplaced,
            "abandon",
            "a reload under an injection would disable a plugin the next page's pass loads",
        ),
        (
            InjectFailure::Undelivered,
            "keep",
            "a UI thread that never took the code says nothing about the plugin",
        ),
    ] {
        assert_eq!(duty(outcome), expected, "{why}");
    }
}

/// A load given up on waits for the pass lock before it turns its plugin off. What happened
/// meanwhile decides whether it still may: a disable written then would land on a plugin that works.
#[test]
fn a_timed_out_load_turns_its_plugin_off_only_if_nothing_replaced_it() {
    for (same_page, last_marked, expected, why) in [
        (
            true,
            Some(4),
            true,
            "nothing changed since the load timed out",
        ),
        (
            true,
            Some(5),
            false,
            "an enable loaded the plugin again since",
        ),
        (
            false,
            Some(4),
            false,
            "the page changed: the next page's pass loads it",
        ),
    ] {
        assert_eq!(timeout_owed(same_page, last_marked, 4), expected, "{why}");
    }
}
