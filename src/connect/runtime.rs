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

        let mut records = self.records.lock().unwrap_or_else(|e| e.into_inner());
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
            let mut map = self.records.lock().unwrap_or_else(|e| e.into_inner());
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
#[path = "../../tests/unit/connect/runtime.rs"]
mod tests;
