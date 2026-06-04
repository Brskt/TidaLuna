//! Registry of in-flight WS request futures.
//!
//! When the client issues a request with a `requestId`, a oneshot channel is
//! registered here. When the peer's response arrives with the matching
//! `requestId`, the response value is forwarded back to the awaiting caller.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

/// Upper bound on how long an unanswered request may sit in the map. A caller
/// that drops its future without timing out (e.g. an aborted `select!`) leaves
/// its sender behind; without this it would only be reclaimed at disconnect.
const PENDING_MAX_AGE: Duration = Duration::from_secs(120);

pub(crate) struct PendingRequests {
    next_id: AtomicU32,
    pending: Mutex<HashMap<u32, (Instant, oneshot::Sender<serde_json::Value>)>>,
}

impl PendingRequests {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU32::new(0),
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn register(&self) -> (u32, oneshot::Receiver<serde_json::Value>) {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        let now = Instant::now();
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        // Reclaim entries whose caller is gone or that have aged out before
        // inserting, so the map stays bounded by live in-flight requests.
        pending
            .retain(|_, (born, tx)| !tx.is_closed() && now.duration_since(*born) < PENDING_MAX_AGE);
        pending.insert(id, (now, tx));
        (id, rx)
    }

    pub fn resolve(&self, request_id: u32, response: serde_json::Value) -> bool {
        if let Some((_, tx)) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&request_id)
        {
            tx.send(response).is_ok()
        } else {
            false
        }
    }

    pub fn remove(&self, request_id: u32) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&request_id);
    }

    pub fn fail_all(&self) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}
