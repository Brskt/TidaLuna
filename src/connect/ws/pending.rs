//! Registry of in-flight WS request futures.
//!
//! When the client issues a request with a `requestId`, a oneshot channel is
//! registered here. When the peer's response arrives with the matching
//! `requestId`, the response value is forwarded back to the awaiting caller.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicU32;

use tokio::sync::oneshot;

pub(crate) struct PendingRequests {
    next_id: AtomicU32,
    pending: Mutex<HashMap<u32, oneshot::Sender<serde_json::Value>>>,
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
        self.pending.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    pub fn resolve(&self, request_id: u32, response: serde_json::Value) -> bool {
        if let Some(tx) = self.pending.lock().unwrap().remove(&request_id) {
            tx.send(response).is_ok()
        } else {
            false
        }
    }

    pub fn remove(&self, request_id: u32) {
        self.pending.lock().unwrap().remove(&request_id);
    }

    pub fn fail_all(&self) {
        self.pending.lock().unwrap().clear();
    }
}
