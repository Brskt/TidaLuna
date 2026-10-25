//! Tests for `src/ipc/plugin/trust_prompt.rs`, attached by `#[path]`.

use super::*;

/// What uninstall does to the prompts: every slot whose key is one of the plugin's modules.
fn uninstall(prompts: &Prompts, module: &str) {
    let prefix = format!("{module}::");
    prompts.abandon_where(|key| key.starts_with(&prefix));
}

fn open(prompts: &Prompts, load_id: u64) -> (Waiting, Option<Answerer>) {
    let entered = prompts.enter("X", "fs", load_id);
    (entered.waiting, entered.answerer)
}

/// Where a slot stands, without waiting on it.
fn peek(waiting: &Waiting) -> Option<Decision> {
    waiting.state.current()
}

fn allowed() -> Decision {
    Decision::Answered([true].into())
}

/// Its window never came up: the slot ends `Abandoned`, which records nothing.
#[tokio::test]
async fn a_dialog_that_never_answered_is_not_a_refusal() {
    let prompts = Prompts::default();
    let (mut waiting, answerer) = open(&prompts, 1);

    answerer.expect("opener").withdraw();

    assert_eq!(waiting.decide().await, Decision::Abandoned);
}

#[tokio::test]
async fn a_late_click_cannot_turn_an_abandoned_prompt_into_a_grant() {
    let prompts = Prompts::default();
    let (mut waiting, answerer) = open(&prompts, 1);

    uninstall(&prompts, "X");
    answerer.expect("opener").answer(vec![true]);

    assert_eq!(waiting.decide().await, Decision::Abandoned);
}

/// The click lands and the waiter reads it, then the uninstall runs on the actor ahead of the
/// waiter's write. The write asks its gate there, and the gate is shut.
#[tokio::test]
async fn a_grant_read_before_the_uninstall_is_not_written_after_it() {
    let prompts = Prompts::default();
    let (mut waiting, answerer) = open(&prompts, 1);
    answerer.expect("opener").answer(vec![true]);
    assert_eq!(waiting.decide().await, allowed());
    let gate = waiting.gate();

    uninstall(&prompts, "X");

    assert!(!gate.is_open());
}

/// The plugin was replaced by a new load while its dialog was open.
#[tokio::test]
async fn a_dialog_of_the_previous_load_does_not_answer_the_new_load() {
    let prompts = Prompts::default();
    let (_old_waiting, old_answerer) = open(&prompts, 1);
    let (new_waiting, new_answerer) = open(&prompts, 2);
    assert!(new_answerer.is_some(), "the new load opens its own dialog");

    old_answerer.expect("opener").answer(vec![true]);

    assert_eq!(peek(&new_waiting), None);
}

/// The attempt of the load that was replaced is not left waiting on a dialog nobody is meant to
/// answer.
#[tokio::test]
async fn a_superseded_attempt_is_released() {
    let prompts = Prompts::default();
    let (old_waiting, _old_answerer) = open(&prompts, 1);
    let (_new_waiting, _new_answerer) = open(&prompts, 2);

    assert_eq!(peek(&old_waiting), Some(Decision::Abandoned));
}

#[tokio::test]
async fn one_load_shares_one_dialog() {
    let prompts = Prompts::default();
    let (first, answerer) = open(&prompts, 1);
    let (second, second_answerer) = open(&prompts, 1);
    assert!(second_answerer.is_none(), "the second attempt joins");

    answerer.expect("opener").answer(vec![true]);

    assert_eq!(peek(&first), Some(allowed()));
    assert_eq!(peek(&second), Some(allowed()));
}

#[tokio::test]
async fn a_finished_waiter_never_removes_a_newer_prompt() {
    let prompts = Prompts::default();
    let (old_waiting, _old_answerer) = open(&prompts, 1);
    uninstall(&prompts, "X");
    let (_new_waiting, new_answerer) = open(&prompts, 2);
    assert!(new_answerer.is_some());

    prompts.finish(&old_waiting);

    let (_third_waiting, third_answerer) = open(&prompts, 2);
    assert!(
        third_answerer.is_none(),
        "the load's prompt is still shared"
    );
}

#[test]
fn a_decision_is_recordable_and_an_abandonment_is_not() {
    assert_eq!(
        recordable(Decision::Answered([true, false].into())).as_deref(),
        Some(&[true, false][..])
    );
    assert_eq!(recordable(Decision::Abandoned), None);
}
