pub(crate) mod bridge;
pub(crate) mod consts;
pub(crate) mod controller;
pub(crate) mod ipc;
pub(crate) mod mdns;
pub(crate) mod receiver;
pub(crate) mod runtime;
#[cfg(test)]
pub(crate) mod testing;
pub(crate) mod token_state;
pub(crate) mod types;
pub(crate) mod ws;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use controller::TidalConnectController;
use mdns::advertiser::MdnsAdvertiser;
use mdns::browser::{BrowserEvent, MdnsBrowser};
use receiver::ConnectReceiver;
use receiver::speaker_bridge::BridgeEvent;
use runtime::TaskGroup;
use types::ReceiverConfig;

/// Graceful budget for tearing Connect down on process exit: just enough to
/// unregister mDNS and send WS close frames before aborting (the OS reclaims the rest).
pub(crate) const EXIT_SHUTDOWN_BUDGET: Duration = Duration::from_millis(250);

pub(crate) struct ConnectManager {
    controller: Option<Arc<Mutex<TidalConnectController>>>,
    receiver: Option<ConnectReceiver>,
    bridge_tx: Option<mpsc::Sender<BridgeEvent>>,
    controller_tasks: Arc<TaskGroup>,
}

impl ConnectManager {
    pub(crate) fn new() -> Self {
        Self {
            controller: None,
            receiver: None,
            bridge_tx: None,
            controller_tasks: Arc::new(TaskGroup::new()),
        }
    }

    /// Initialize the controller (mDNS browser) and spawn the browser event loop.
    pub(crate) fn init_controller(&mut self) -> anyhow::Result<()> {
        if self.controller.is_some() {
            return Ok(());
        }

        let (event_tx, mut event_rx) = mpsc::channel::<BrowserEvent>(64);
        let browser = MdnsBrowser::new(event_tx, true)?;
        let controller = Arc::new(Mutex::new(TidalConnectController::new(Some(browser))));
        self.controller = Some(controller.clone());

        // Spawn browser event polling task on the tokio runtime
        // (init_controller may be called from the CEF UI thread, not a tokio context).
        // The TaskGroup requires a tokio context at spawn time, so enter the
        // runtime before invoking it.
        let ctrl = controller.clone();
        let Some(rt) = crate::state::RT_HANDLE.get() else {
            anyhow::bail!("Tokio runtime not available");
        };
        let _guard = rt.enter();
        self.controller_tasks
            .spawn("controller-browser-loop", async move {
                while let Some(event) = event_rx.recv().await {
                    let devices = {
                        let mut guard = ctrl.lock().unwrap();
                        guard.handle_browser_event(event);
                        guard.discovered_devices().to_vec()
                    };
                    // Emit to frontend - must post to CEF UI thread
                    ipc::post_emit_with_data("connect.devices_received", &devices);
                }
            })?;

        crate::vprintln!("[connect] Controller initialized");
        Ok(())
    }

    /// Build a receiver (WS server + mDNS advertiser) without touching `self`.
    ///
    /// The async work borrows nothing from `AppState`, so the caller can run it
    /// without holding any lock and then hand the result to [`install_receiver`]
    /// in a synchronous step. This is what keeps the manager from being moved
    /// out of `AppState` across an await (the old `take()`/restore left a window
    /// where `connect` was `None` and concurrent calls could drop a manager).
    ///
    /// [`install_receiver`]: Self::install_receiver
    pub(crate) async fn build_receiver(
        config: ReceiverConfig,
    ) -> anyhow::Result<(ConnectReceiver, mpsc::Sender<BridgeEvent>)> {
        let advertiser = MdnsAdvertiser::new().ok();
        ConnectReceiver::start(config, advertiser).await
    }

    /// Install a freshly built receiver and route bridge events to it.
    /// Synchronous: no await, so it runs entirely under the `AppState` lock.
    pub(crate) fn install_receiver(
        &mut self,
        receiver: ConnectReceiver,
        bridge_tx: mpsc::Sender<BridgeEvent>,
    ) {
        self.receiver = Some(receiver);
        self.bridge_tx = Some(bridge_tx.clone());
        crate::connect::bridge::set_active(Some(bridge_tx));
    }

    /// Detach the active receiver (if any) and stop routing bridge events.
    /// Synchronous: the returned receiver must be `shutdown().await`ed by the
    /// caller outside the `AppState` lock.
    pub(crate) fn take_receiver(&mut self) -> Option<ConnectReceiver> {
        crate::connect::bridge::set_active(None);
        self.bridge_tx = None;
        self.receiver.take()
    }

    pub(crate) fn controller(&self) -> Option<&Arc<Mutex<TidalConnectController>>> {
        self.controller.as_ref()
    }

    pub(crate) fn is_receiver_active(&self) -> bool {
        self.receiver.is_some()
    }

    pub(crate) fn get_state_snapshot(&self) -> serde_json::Value {
        let mut snapshot = serde_json::json!({
            "devices": [],
            "isConnected": false,
            "receiverActive": self.receiver.is_some(),
        });

        if let Some(ref ctrl) = self.controller {
            let guard = ctrl.lock().unwrap();
            snapshot["devices"] =
                serde_json::to_value(guard.discovered_devices()).unwrap_or_default();
            snapshot["isConnected"] = serde_json::Value::Bool(guard.is_connected());

            if let Some((session_id, device, joined)) = guard.session_info() {
                snapshot["session"] = serde_json::json!({
                    "sessionId": session_id,
                    "device": device,
                    "joined": joined,
                });
            }
            if let Some(media) = guard.last_media() {
                snapshot["media"] = serde_json::to_value(media).unwrap_or_default();
            }
            let player_state = guard.last_player_state();
            let progress = guard.last_progress();
            if player_state != crate::connect::types::PlayerState::Idle {
                snapshot["playerStatus"] = serde_json::json!({
                    "playerState": player_state,
                    "progress": progress,
                });
            }
        }

        snapshot
    }

    /// Tear down receiver + controller within `deadline` total graceful budget.
    pub(crate) async fn shutdown(&mut self, deadline: Duration) {
        if let Some(mut receiver) = self.take_receiver() {
            receiver.shutdown(deadline).await;
        }
        let report = self.controller_tasks.shutdown(deadline).await;
        if !report.panicked.is_empty() {
            crate::vprintln!(
                "[connect] Controller task panics on shutdown: {:?}",
                report.panicked
            );
        }
        if let Some(ctrl) = self.controller.take()
            && let Ok(mut guard) = ctrl.lock()
        {
            guard.shutdown();
        }
        crate::vprintln!("[connect] Shutdown complete");
    }
}
