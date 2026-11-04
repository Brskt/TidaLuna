//! The owner of one mdns-sd daemon thread.

use std::ops::Deref;
use std::sync::Arc;

use mdns_sd::ServiceDaemon;

/// A `ServiceDaemon` that stops with its owner. mdns-sd has no `Drop` of its own; its thread
/// holds a sender to its own command channel and ends only on `Exit`, leaving a dropped owner's
/// daemon running with its sockets until exit.
pub(crate) struct OwnedDaemon(Arc<ServiceDaemon>);

impl OwnedDaemon {
    pub(crate) fn new() -> mdns_sd::Result<Self> {
        ServiceDaemon::new().map(|daemon| Self(Arc::new(daemon)))
    }

    /// A handle for a bounded, awaited shutdown (`MdnsBackend`); the owner still stops it on drop.
    pub(crate) fn share(&self) -> Arc<ServiceDaemon> {
        self.0.clone()
    }
}

impl Deref for OwnedDaemon {
    type Target = ServiceDaemon;

    fn deref(&self) -> &ServiceDaemon {
        &self.0
    }
}

impl Drop for OwnedDaemon {
    fn drop(&mut self) {
        // Queues the command and returns: safe on a runtime thread. The daemon says goodbye
        // for its services on `Exit`. `DaemonShutdown` is the normal second call (already stopped);
        // anything else means the command was refused and the thread may still run.
        match self.0.shutdown() {
            Ok(_) | Err(mdns_sd::Error::DaemonShutdown) => {}
            Err(e) => crate::verr!("[connect::mdns] Daemon shutdown refused: {e}"),
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/connect/mdns/daemon.rs"]
mod tests;
