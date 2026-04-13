pub(crate) mod client_comm;
pub(crate) mod playback;
pub(crate) mod queue;
pub(crate) mod session;
pub(crate) mod speaker_bridge;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::connect::mdns::advertiser::{AdvertiseConfig, MdnsAdvertiser};
use crate::connect::types::ReceiverConfig;
use crate::connect::ws::server::{IncomingMessage, ServerEvent, WsServer};
use cef::*;

use client_comm::{ClientCommunicator, DispatchedCommand, classify_command};
use playback::{PlaybackController, PlaybackInternalEvent, PlaybackNotifyEvent};
use queue::{QueueManager, QueueNotifyEvent};
use session::{ReceiverSession, SessionInternalEvent, SessionNotifyEvent};
use speaker_bridge::{BridgeEvent, SpeakerBridge};

pub(crate) struct ConnectReceiver {
    cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    advertiser: Option<MdnsAdvertiser>,
}

impl ConnectReceiver {
    pub async fn start(
        config: ReceiverConfig,
        advertiser: Option<MdnsAdvertiser>,
    ) -> anyhow::Result<(Self, mpsc::Sender<BridgeEvent>)> {
        let cancel = CancellationToken::new();

        // Channels
        let (incoming_tx, incoming_rx) = mpsc::channel::<IncomingMessage>(128);
        let (server_event_tx, server_event_rx) = mpsc::channel::<ServerEvent>(32);
        let (session_tx, session_rx) = mpsc::channel::<SessionNotifyEvent>(64);
        let (playback_tx, playback_rx) = mpsc::channel::<PlaybackNotifyEvent>(64);
        let (queue_tx, queue_rx) = mpsc::channel::<QueueNotifyEvent>(64);
        let (bridge_tx, bridge_rx) = mpsc::channel::<BridgeEvent>(128);

        // WS server
        let server = WsServer::start(config.ws_port, incoming_tx, server_event_tx).await?;

        // mDNS advertiser
        let mut adv = advertiser;
        if let Some(ref mut a) = adv {
            let adv_config = AdvertiseConfig {
                friendly_name: config.friendly_name.clone(),
                model_name: config.model_name.clone(),
                port: config.ws_port,
                ..AdvertiseConfig::default()
            };
            if let Err(e) = a.start(&adv_config) {
                crate::vprintln!("[connect::receiver] mDNS advertise failed: {}", e);
            }
        }

        // Services
        let session_mgr = ReceiverSession::new(session_tx);
        let bridge = SpeakerBridge::new(bridge_tx.clone());
        let playback_ctrl = PlaybackController::new(bridge, playback_tx);
        let queue_mgr = QueueManager::new(reqwest::Client::new(), queue_tx);
        let client_comm = ClientCommunicator::new(server);

        // Main routing loop with all 6 channel arms
        let routing_cancel = cancel.clone();
        let routing_task = tokio::spawn(routing_loop(
            client_comm,
            session_mgr,
            playback_ctrl,
            queue_mgr,
            bridge_rx,
            incoming_rx,
            server_event_rx,
            session_rx,
            playback_rx,
            queue_rx,
            routing_cancel,
        ));

        crate::vprintln!(
            "[connect::receiver] Started on port {} as '{}'",
            config.ws_port,
            config.friendly_name,
        );

        Ok((
            Self {
                cancel,
                tasks: vec![routing_task],
                advertiser: adv,
            },
            bridge_tx,
        ))
    }

    pub async fn shutdown(&mut self) {
        self.cancel.cancel();
        if let Some(ref mut adv) = self.advertiser {
            adv.stop();
        }
        for task in self.tasks.drain(..) {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        }
        crate::vprintln!("[connect::receiver] Shut down");
    }
}

// ── Routing loop ─────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn routing_loop(
    mut client_comm: ClientCommunicator,
    mut session_mgr: ReceiverSession,
    mut playback_ctrl: PlaybackController,
    mut queue_mgr: QueueManager,
    mut bridge_rx: mpsc::Receiver<BridgeEvent>,
    mut incoming_rx: mpsc::Receiver<IncomingMessage>,
    mut server_event_rx: mpsc::Receiver<ServerEvent>,
    mut session_rx: mpsc::Receiver<SessionNotifyEvent>,
    mut playback_rx: mpsc::Receiver<PlaybackNotifyEvent>,
    mut queue_rx: mpsc::Receiver<QueueNotifyEvent>,
    cancel: CancellationToken,
) {
    crate::vprintln!("[connect::receiver] Routing loop started");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,

            // Arm 1: Incoming WS messages from mobile clients
            Some(incoming) = incoming_rx.recv() => {
                let command = incoming.message.get("command")
                    .and_then(|v| v.as_str()).unwrap_or("").to_string();
                let request_id = incoming.message.get("requestId").and_then(|v| v.as_u64());
                let sid = incoming.socket_id;
                crate::vprintln!("[connect::receiver] <- {} (socket {}, rid={:?})", command, sid, request_id);
                let needs_ack = match classify_command(sid, &command, incoming.message) {
                    DispatchedCommand::Session { socket_id, command, payload } => {
                        session_mgr.handle_command(socket_id, &command, &payload).await;
                        false // Sessions have their own notification protocol
                    }
                    DispatchedCommand::Playback { command, payload } => {
                        dispatch_playback(&mut playback_ctrl, &command, &payload).await;
                        true
                    }
                    DispatchedCommand::Queue { socket_id, command, payload } => {
                        queue_mgr.handle_command(socket_id, &command, &payload).await;
                        true
                    }
                };
                if needs_ack
                    && let Some(rid) = request_id
                {
                    let reply = serde_json::json!({
                        "command": "notifyRequestResult",
                        "requestId": rid,
                        "resultCode": 0,
                    });
                    let _ = client_comm.send_to(sid, &reply).await;
                }
            }

            // Arm 2: Server events (client connect/disconnect)
            Some(event) = server_event_rx.recv() => {
                match event {
                    ServerEvent::ClientConnected(socket_id) => {
                        crate::vprintln!("[connect::receiver] Client {} connected", socket_id);
                    }
                    ServerEvent::ClientDisconnected(socket_id) => {
                        session_mgr.handle_disconnect(socket_id).await;
                    }
                }
            }

            // Arm 3: Session notifications -> relay to WS + handle internals
            Some(event) = session_rx.recv() => {
                match event {
                    SessionNotifyEvent::Broadcast(ref msg) => {
                        client_comm.broadcast(msg).await;
                    }
                    SessionNotifyEvent::Unicast { socket_id, ref message } => {
                        let _ = client_comm.send_to(socket_id, message).await;
                    }
                    SessionNotifyEvent::Internal(ref internal) => {
                        match internal {
                            SessionInternalEvent::SessionEnded => {
                                playback_ctrl.reset().await;
                                crate::connect::ipc::post_emit("connect.receiver.session_ended");
                            }
                            SessionInternalEvent::SessionStarted { session_id } => {
                                crate::vprintln!("[connect::receiver] Session started: {}", session_id);
                            }
                            SessionInternalEvent::ResourcesGranted
                            | SessionInternalEvent::ResourcesRevoked => {}
                        }
                    }
                }
            }

            // Arm 4: Playback notifications -> relay to WS + handle internals
            Some(event) = playback_rx.recv() => {
                match event {
                    PlaybackNotifyEvent::Broadcast(ref msg) => {
                        client_comm.broadcast(msg).await;
                    }
                    PlaybackNotifyEvent::Internal(ref internal) => {
                        match internal {
                            PlaybackInternalEvent::MediaCompleted { has_next } => {
                                if *has_next {
                                    queue_mgr.on_request_next_media().await;
                                } else {
                                    queue_mgr.on_media_ended().await;
                                }
                            }
                        }
                    }
                }
            }

            // Arm 5: Queue notifications -> relay to WS + dispatch to playback
            Some(event) = queue_rx.recv() => {
                match event {
                    QueueNotifyEvent::Broadcast(ref msg) => {
                        client_comm.broadcast(msg).await;
                    }
                    QueueNotifyEvent::SetMedia { ref media_info, media_seq_no } => {
                        crate::vprintln!("[connect::receiver] SetMedia: {} (seq={})", media_info.item_id, media_seq_no);
                        playback_ctrl.set_media(media_info.clone(), media_seq_no).await;
                        // Update the local frontend UI with the new track metadata
                        emit_receiver_metadata(media_info);
                    }
                    QueueNotifyEvent::SetNextMedia { media_info, media_seq_no } => {
                        playback_ctrl.set_next_media(media_info, media_seq_no).await;
                    }
                    QueueNotifyEvent::SetVolume { level } => {
                        playback_ctrl.set_volume(level).await;
                        // Notify frontend to update volume slider (Connect uses 0.0-1.0)
                        let volume_pct = (level * 100.0).clamp(0.0, 100.0);
                        crate::connect::ipc::post_emit_with_data(
                            "connect.receiver.volume_changed",
                            &serde_json::json!({ "volume": volume_pct }),
                        );
                    }
                    QueueNotifyEvent::SetMute { mute } => {
                        playback_ctrl.set_mute(mute).await;
                        crate::connect::ipc::post_emit_with_data(
                            "connect.receiver.mute_changed",
                            &serde_json::json!({ "mute": mute }),
                        );
                    }
                    QueueNotifyEvent::QueueGenBumped { queue_gen } => {
                        playback_ctrl.set_queue_gen(queue_gen);
                    }
                }
            }

            // Arm 6: Bridge events from local player
            Some(event) = bridge_rx.recv() => {
                match event {
                    BridgeEvent::Prepared { engine_gen } => {
                        crate::vprintln!("[connect::bridge] <- Prepared (gen={})", engine_gen);
                        playback_ctrl.on_prepared(engine_gen).await;
                    }
                    BridgeEvent::StatusUpdated { ref state, engine_gen } => {
                        crate::vprintln!("[connect::bridge] <- StatusUpdated {:?} (gen={})", state, engine_gen);
                        playback_ctrl.on_status_updated(*state, engine_gen).await;
                    }
                    BridgeEvent::ProgressUpdated { progress_ms, duration_ms, engine_gen } => {
                        // Don't log every progress (too noisy) - logged once via flush.rs
                        playback_ctrl.on_progress_updated(progress_ms, duration_ms, engine_gen).await;
                    }
                    BridgeEvent::PlaybackCompleted { has_next_media, engine_gen } => {
                        crate::vprintln!("[connect::bridge] <- PlaybackCompleted has_next={} (gen={})", has_next_media, engine_gen);
                        let _ = has_next_media;
                        playback_ctrl.on_playback_completed(engine_gen).await;
                    }
                    BridgeEvent::PlaybackError { ref status_code, engine_gen } => {
                        crate::vprintln!("[connect::bridge] <- PlaybackError {} (gen={})", status_code, engine_gen);
                        playback_ctrl.on_playback_error(status_code, engine_gen).await;
                    }
                }
            }

            else => break,
        }
    }

    client_comm.shutdown().await;
    crate::vprintln!("[connect::receiver] Routing loop ended");
}

/// Dispatch a playback command from a mobile client to the PlaybackController.
async fn dispatch_playback(
    ctrl: &mut PlaybackController,
    command: &str,
    payload: &serde_json::Value,
) {
    match command {
        "play" => ctrl.play().await,
        "pause" => ctrl.pause().await,
        "stop" => ctrl.stop().await,
        "seek" => {
            crate::vprintln!("[connect::playback] seek payload: {}", payload);
            let pos = payload
                .get("position")
                .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
                .unwrap_or(0);
            crate::vprintln!("[connect::playback] seek to {}ms", pos);
            ctrl.seek(pos).await;
        }
        _ => {}
    }
}

/// Push track metadata to the local frontend when a Connect receiver loads a new track.
fn emit_receiver_metadata(media: &crate::connect::types::MediaInfo) {
    // Update OS media controls (SMTC)
    let title = media
        .metadata
        .as_ref()
        .and_then(|m| m.title.as_deref())
        .unwrap_or("Unknown");
    let artist = media
        .metadata
        .as_ref()
        .and_then(|m| m.artists.as_ref())
        .and_then(|a| {
            if let Some(arr) = a.as_array() {
                arr.first().and_then(|v| v.as_str())
            } else {
                a.as_str()
            }
        })
        .unwrap_or("Unknown");
    let duration_ms = media
        .metadata
        .as_ref()
        .and_then(|m| m.duration)
        .unwrap_or(0);
    let duration_secs = if duration_ms > 0 {
        Some(duration_ms as f64 / 1000.0)
    } else {
        None
    };
    // Post to CEF UI thread - OsMediaControls (SMTC) requires UI thread access.
    let mut task = ReceiverMetadataTask::new(title.to_string(), artist.to_string(), duration_secs);
    post_task(ThreadId::UI, Some(&mut task));

    // Emit full MediaInfo to frontend for player bar + Redux updates
    crate::connect::ipc::post_emit_with_data("connect.receiver.media_changed", media);
}

wrap_task! {
    struct ReceiverMetadataTask {
        title: String,
        artist: String,
        duration_secs: Option<f64>,
    }
    impl Task {
        fn execute(&self) {
            crate::app_state::with_state(|state| {
                if let Some(ref mut mc) = state.media_controls {
                    mc.set_metadata(&self.title, &self.artist, self.duration_secs);
                }
            });
        }
    }
}
