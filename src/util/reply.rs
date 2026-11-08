use std::sync::Mutex;

use tokio::sync::oneshot;

/// The one answer a request gets, shared by every path that can give it: the first `send`
/// delivers, later ones are dropped.
pub(crate) struct OnceReply<R>(Mutex<Option<oneshot::Sender<R>>>);

impl<R> OnceReply<R> {
    pub(crate) fn new(tx: oneshot::Sender<R>) -> Self {
        Self(Mutex::new(Some(tx)))
    }

    pub(crate) fn send(&self, answer: R) {
        if let Some(tx) = self.0.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = tx.send(answer);
        }
    }
}
