//! Structured task management for the connect module.
//!
//! `TaskGroup` owns every async task spawned on behalf of the module and
//! reports a classified outcome for each one at shutdown: completed, aborted,
//! panicked, or still running past its deadline (for unabortable blocking
//! work). This makes shutdown deterministic and observable, so a crash in one
//! subsystem cannot leave orphan tasks holding network sockets or locks.

// Panic reporting via JoinError::into_panic only works when the binary is
// compiled with panic = "unwind"; panic = "abort" terminates the process
// before tokio can classify the join outcome. scripts/check-panic-profile.sh
// guards Cargo.toml against profile regressions at the repo level.
const _: () = assert!(
    cfg!(panic = "unwind"),
    "src/connect/runtime.rs requires panic = \"unwind\" for ShutdownReport.panicked accounting",
);

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::{AbortHandle, JoinHandle};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

// ---------------------------------------------------------------------------
// Task state
// ---------------------------------------------------------------------------

/// Lifecycle state of a single tracked task.
///
/// Encoded as `u8` for atomic storage. Transitions are enforced through
/// `try_transition` (compare_exchange), not raw `store`, so a terminal
/// classification can never be overwritten by a late `AbortRequested` write.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum TaskState {
    Running = 0,
    AbortRequested = 1,
    AbortObserved = 2,
    PanicObserved = 3,
    GracefulCompleted = 4,
}

impl TaskState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => TaskState::Running,
            1 => TaskState::AbortRequested,
            2 => TaskState::AbortObserved,
            3 => TaskState::PanicObserved,
            4 => TaskState::GracefulCompleted,
            _ => unreachable!("invalid TaskState byte {v}"),
        }
    }

    fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::AbortObserved | TaskState::PanicObserved | TaskState::GracefulCompleted
        )
    }

    /// Legal transition matrix. Terminal states are sinks.
    fn can_transition(from: Self, to: Self) -> bool {
        use TaskState::*;
        matches!(
            (from, to),
            (Running, AbortRequested)
                | (Running, PanicObserved)
                | (Running, GracefulCompleted)
                | (Running, AbortObserved)
                | (AbortRequested, AbortObserved)
                | (AbortRequested, PanicObserved)
                | (AbortRequested, GracefulCompleted)
        )
    }
}

/// CAS a task state toward `to`. Returns `true` if the transition was applied
/// (or if the current value already matches `to`). Returns `false` only when
/// the current state cannot legally reach `to` (e.g. already terminal).
fn try_transition(atom: &AtomicU8, to: TaskState) -> bool {
    loop {
        let current_byte = atom.load(Ordering::SeqCst);
        let current = TaskState::from_u8(current_byte);
        if current == to {
            return true;
        }
        if !TaskState::can_transition(current, to) {
            return false;
        }
        match atom.compare_exchange(current_byte, to as u8, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(_) => continue,
        }
    }
}

// ---------------------------------------------------------------------------
// Task record
// ---------------------------------------------------------------------------

struct TaskRecord {
    handle: JoinHandle<()>,
    abort: AbortHandle,
    /// Shared with the task wrapper so the wrapper can mark
    /// `GracefulCompleted` from inside the task.
    state: Arc<AtomicU8>,
}

// ---------------------------------------------------------------------------
// Unabortable handle (blocking tasks outside tokio's cancellation model)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Errors and report
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum SpawnError {
    /// `shutdown` has been called; no further tasks may be spawned.
    GroupClosed,
    /// Another task with the same name is already tracked.
    DuplicateName(&'static str),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::GroupClosed => f.write_str("TaskGroup is closed, cannot spawn"),
            SpawnError::DuplicateName(n) => write!(f, "task name '{n}' is already tracked"),
        }
    }
}

impl std::error::Error for SpawnError {}

/// Final classification of every task tracked by a `TaskGroup` after
/// `shutdown` returns.
#[derive(Debug, Default)]
pub struct ShutdownReport {
    pub graceful_completed: Vec<&'static str>,
    pub aborted: Vec<&'static str>,
    pub panicked: Vec<(&'static str, String)>,
}

// ---------------------------------------------------------------------------
// TaskGroup
// ---------------------------------------------------------------------------

pub struct TaskGroup {
    tracker: TaskTracker,
    cancel: CancellationToken,
    records: Mutex<HashMap<&'static str, TaskRecord>>,
    closed: AtomicBool,
}

impl TaskGroup {
    pub fn new() -> Self {
        Self {
            tracker: TaskTracker::new(),
            cancel: CancellationToken::new(),
            records: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        }
    }

    /// Shared cancellation token. Long-running tasks should `select!` on this
    /// to observe graceful shutdown before `abort()` is issued.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Spawn a named async task. The name is used for the `ShutdownReport`
    /// classification and must be unique within the group.
    pub fn spawn<F>(&self, name: &'static str, fut: F) -> Result<(), SpawnError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if self.closed.load(Ordering::SeqCst) {
            return Err(SpawnError::GroupClosed);
        }

        let state = Arc::new(AtomicU8::new(TaskState::Running as u8));
        let state_for_task = state.clone();

        // Wrap the user future so the wrapper writes GracefulCompleted on
        // normal exit. If the task panics, the wrapper never reaches the
        // final transition; shutdown() detects the panic via
        // JoinError::try_into_panic.
        let wrapped = async move {
            fut.await;
            try_transition(&state_for_task, TaskState::GracefulCompleted);
        };

        let handle = self.tracker.spawn(wrapped);
        let abort = handle.abort_handle();
        let record = TaskRecord {
            handle,
            abort,
            state,
        };

        let mut records = self.records.lock().unwrap();
        if records.contains_key(name) {
            // Abort the already-spawned task to avoid orphans.
            record.abort.abort();
            return Err(SpawnError::DuplicateName(name));
        }
        records.insert(name, record);
        Ok(())
    }

    /// Two-phase shutdown:
    /// 1. Close the spawn gate, cancel the shared token, wait up to
    ///    `graceful_timeout` for cooperating tasks to exit.
    /// 2. Abort whatever is still running, drain JoinHandles, classify each
    ///    task via the shared atomic state and `JoinError`.
    pub async fn shutdown(&self, graceful_timeout: Duration) -> ShutdownReport {
        self.closed.store(true, Ordering::SeqCst);
        self.tracker.close();
        self.cancel.cancel();

        let _ = tokio::time::timeout(graceful_timeout, self.tracker.wait()).await;

        let mut report = ShutdownReport::default();

        let records: Vec<(&'static str, TaskRecord)> = {
            let mut map = self.records.lock().unwrap();
            map.drain().collect()
        };

        for (name, record) in records {
            let was_terminal =
                TaskState::from_u8(record.state.load(Ordering::SeqCst)).is_terminal();
            if !was_terminal {
                record.abort.abort();
                try_transition(&record.state, TaskState::AbortRequested);
            }

            match record.handle.await {
                Ok(()) => {
                    let final_state = TaskState::from_u8(record.state.load(Ordering::SeqCst));
                    if final_state == TaskState::GracefulCompleted {
                        report.graceful_completed.push(name);
                    } else {
                        // The task returned Ok but never wrote
                        // GracefulCompleted, meaning the abort raced with a
                        // natural completion. Treat as aborted.
                        try_transition(&record.state, TaskState::AbortObserved);
                        report.aborted.push(name);
                    }
                }
                Err(join_err) if join_err.is_cancelled() => {
                    try_transition(&record.state, TaskState::AbortObserved);
                    report.aborted.push(name);
                }
                Err(join_err) => {
                    let msg = match join_err.try_into_panic() {
                        Ok(payload) => panic_payload_to_string(payload),
                        Err(_) => "task failed without panic payload".to_string(),
                    };
                    try_transition(&record.state, TaskState::PanicObserved);
                    report.panicked.push((name, msg));
                }
            }
        }

        report
    }
}

impl Default for TaskGroup {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn panic_payload_to_string(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
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
}
