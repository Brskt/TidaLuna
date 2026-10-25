//! Tests for `src/connect/runtime.rs`, attached to it by `#[path]`.

use super::*;

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
async fn a_dropped_group_cancels_and_aborts_its_tasks() {
    let group = TaskGroup::new();
    let token = group.cancel_token();
    let (held, released) = tokio::sync::oneshot::channel::<()>();
    group
        .spawn("holder", async move {
            let _held = held;
            std::future::pending::<()>().await;
        })
        .unwrap();
    drop(group);
    let outcome = tokio::time::timeout(Duration::from_secs(1), released).await;
    assert!(matches!(outcome, Ok(Err(_))), "the task outlived its group");
    assert!(
        token.is_cancelled(),
        "the group's token was left uncancelled"
    );
}
