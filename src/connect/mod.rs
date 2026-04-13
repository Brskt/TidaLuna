pub(crate) mod controller;
pub(crate) mod ipc;
pub(crate) mod mdns;
pub(crate) mod receiver;
pub(crate) mod types;
pub(crate) mod ws;

use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use controller::TidalConnectController;
use mdns::advertiser::MdnsAdvertiser;
use mdns::browser::{BrowserEvent, MdnsBrowser};
use receiver::ConnectReceiver;
use receiver::speaker_bridge::BridgeEvent;
use types::ReceiverConfig;

pub(crate) struct ConnectManager {
    controller: Option<Arc<Mutex<TidalConnectController>>>,
    receiver: Option<ConnectReceiver>,
    bridge_tx: Option<mpsc::Sender<BridgeEvent>>,
    controller_tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl ConnectManager {
    pub(crate) fn new() -> Self {
        Self {
            controller: None,
            receiver: None,
            bridge_tx: None,
            controller_tasks: Vec::new(),
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
        // (init_controller may be called from the CEF UI thread, not a tokio context)
        let ctrl = controller.clone();
        let Some(rt) = crate::state::RT_HANDLE.get() else {
            anyhow::bail!("Tokio runtime not available");
        };
        let task = rt.spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let devices = {
                    let mut guard = ctrl.lock().unwrap();
                    guard.handle_browser_event(event);
                    guard.discovered_devices().to_vec()
                };
                // Emit to frontend - must post to CEF UI thread
                ipc::post_emit_with_data("connect.devices_received", &devices);
            }
        });
        self.controller_tasks.push(task);

        crate::vprintln!("[connect] Controller initialized");
        Ok(())
    }

    /// Start the receiver (WS server + mDNS advertiser).
    pub(crate) async fn start_receiver(&mut self, config: ReceiverConfig) -> anyhow::Result<()> {
        if self.receiver.is_some() {
            return Ok(());
        }

        let advertiser = MdnsAdvertiser::new().ok();
        let (receiver, bridge_tx) = ConnectReceiver::start(config, advertiser).await?;
        self.receiver = Some(receiver);
        self.bridge_tx = Some(bridge_tx.clone());
        crate::ui::flush::set_connect_bridge_tx(Some(bridge_tx));
        Ok(())
    }

    pub(crate) async fn stop_receiver(&mut self) {
        crate::ui::flush::set_connect_bridge_tx(None);
        self.bridge_tx = None;
        if let Some(mut receiver) = self.receiver.take() {
            receiver.shutdown().await;
        }
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

    pub(crate) async fn shutdown(&mut self) {
        self.stop_receiver().await;
        for task in self.controller_tasks.drain(..) {
            task.abort();
        }
        if let Some(ctrl) = self.controller.take()
            && let Ok(mut guard) = ctrl.lock()
        {
            guard.shutdown();
        }
        crate::vprintln!("[connect] Shutdown complete");
    }
}
