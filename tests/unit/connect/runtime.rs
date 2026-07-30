//! Tests for `src/connect/runtime.rs`, attached to it by `#[path]`.

use super::*;

#[tokio::test]
async fn task_runs_and_is_reported_graceful() {
    let group = TaskGroup::new();
    group.spawn("ok", async {}).unwrap();
    let report = group.shutdown(Duration::from_millis(100)).await;
    assert_eq!(report.graceful_completed, vec!["ok"]);
    assert!(report.aborted.is_empty());
    assert!(report.panicked.is_empty());
}

#[tokio::test]
async fn cooperative_task_observes_cancel_and_exits_graceful() {
    let group = TaskGroup::new();
    let cancel = group.cancel_token();
    group
        .spawn("coop", async move {
            cancel.cancelled().await;
        })
        .unwrap();
    let report = group.shutdown(Duration::from_millis(100)).await;
    assert_eq!(report.graceful_completed, vec!["coop"]);
}

#[tokio::test]
async fn non_cooperative_task_is_aborted() {
    let group = TaskGroup::new();
    group
        .spawn("sleeper", async {
            // Long sleep that will outlive the graceful window.
            tokio::time::sleep(Duration::from_secs(60)).await;
        })
        .unwrap();
    let report = group.shutdown(Duration::from_millis(50)).await;
    assert_eq!(report.aborted, vec!["sleeper"]);
    assert!(report.graceful_completed.is_empty());
}

#[tokio::test]
async fn panicking_task_is_reported_panicked() {
    let group = TaskGroup::new();
    group.spawn("boom", async { panic!("kaboom") }).unwrap();
    let report = group.shutdown(Duration::from_millis(100)).await;
    assert_eq!(report.panicked.len(), 1);
    assert_eq!(report.panicked[0].0, "boom");
    assert!(report.panicked[0].1.contains("kaboom"));
    assert!(report.graceful_completed.is_empty());
}

#[tokio::test]
async fn spawn_after_shutdown_is_rejected() {
    let group = TaskGroup::new();
    let _ = group.shutdown(Duration::from_millis(10)).await;
    let err = group.spawn("late", async {}).unwrap_err();
    assert!(matches!(err, SpawnError::GroupClosed));
}

#[tokio::test]
async fn duplicate_name_is_rejected() {
    let group = TaskGroup::new();
    group.spawn("dup", async {}).unwrap();
    let err = group.spawn("dup", async {}).unwrap_err();
    assert!(matches!(err, SpawnError::DuplicateName("dup")));
    let _ = group.shutdown(Duration::from_millis(100)).await;
}

#[test]
fn transition_matrix_forbids_terminal_overwrite() {
    assert!(!TaskState::can_transition(
        TaskState::PanicObserved,
        TaskState::AbortRequested,
    ));
    assert!(!TaskState::can_transition(
        TaskState::GracefulCompleted,
        TaskState::AbortObserved,
    ));
    assert!(TaskState::can_transition(
        TaskState::Running,
        TaskState::GracefulCompleted,
    ));
    assert!(TaskState::can_transition(
        TaskState::AbortRequested,
        TaskState::PanicObserved,
    ));
}

#[test]
fn try_transition_is_no_op_when_already_at_target() {
    let atom = AtomicU8::new(TaskState::GracefulCompleted as u8);
    assert!(try_transition(&atom, TaskState::GracefulCompleted));
    assert_eq!(
        atom.load(Ordering::SeqCst),
        TaskState::GracefulCompleted as u8
    );
}

#[test]
fn try_transition_refuses_terminal_overwrite() {
    let atom = AtomicU8::new(TaskState::PanicObserved as u8);
    assert!(!try_transition(&atom, TaskState::AbortRequested));
    assert_eq!(atom.load(Ordering::SeqCst), TaskState::PanicObserved as u8);
}
