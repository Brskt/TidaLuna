pub(crate) mod client_comm;
pub(crate) mod playback;
pub(crate) mod queue;
pub(crate) mod session;
pub(crate) mod speaker_bridge;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bridge::PlayerBridgeEvent;
use crate::connect::mdns::advertiser::{AdvertiseConfig, MdnsAdvertiser};
use crate::connect::runtime::TaskGroup;
use crate::connect::types::{ReceiverConfig, RequestResult};
use crate::connect::ws::server::{ClientEvent, WsServer};
use cef::*;

use client_comm::{ClientCommunicator, DispatchedCommand, classify_command};
use playback::{PlaybackController, PlaybackInternalEvent, PlaybackNotifyEvent};
use queue::{CastRoute, QueueManager, QueueNotifyEvent, QueueStep};
use session::{ReceiverSession, SessionEnd, SessionInternalEvent, SessionNotifyEvent};
use speaker_bridge::{BridgeEvent, SpeakerBridge};

/// In-session graceful-shutdown budget (receiver toggled off, or orphan cleanup).
/// The exit path passes a shorter budget instead.
pub(crate) const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);

pub(crate) struct ConnectReceiver {
    tasks: Arc<TaskGroup>,
    advertiser: Option<MdnsAdvertiser>,
}

impl ConnectReceiver {
    pub async fn start(
        config: ReceiverConfig,
        advertiser: Option<MdnsAdvertiser>,
    ) -> anyhow::Result<(Self, mpsc::Sender<BridgeEvent>)> {
        // Channels
        let (client_tx, client_rx) = mpsc::channel::<ClientEvent>(128);

        // WS server
        let server = WsServer::start(config.ws_port, client_tx).await?;

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

        let (receiver, bridge_tx) = Self::launch(server, client_rx, reqwest::Client::new(), adv)?;

        crate::vprintln!(
            "[connect::receiver] Started on port {} as '{}'",
            config.ws_port,
            config.friendly_name,
        );

        Ok((receiver, bridge_tx))
    }

    /// The services and the routing task over a started server. The queue's HTTP client is
    /// taken as given so a test can point it at a server of its own.
    fn launch(
        server: WsServer,
        client_rx: mpsc::Receiver<ClientEvent>,
        http: reqwest::Client,
        advertiser: Option<MdnsAdvertiser>,
    ) -> anyhow::Result<(Self, mpsc::Sender<BridgeEvent>)> {
        let tasks = Arc::new(TaskGroup::new());
        let cancel = tasks.cancel_token();

        let (session_tx, session_rx) = mpsc::channel::<SessionNotifyEvent>(64);
        let (playback_tx, playback_rx) = mpsc::channel::<PlaybackNotifyEvent>(64);
        let (queue_tx, queue_rx) = mpsc::channel::<QueueNotifyEvent>(64);
        let (bridge_tx, bridge_rx) = mpsc::channel::<BridgeEvent>(128);

        // Services
        let session_mgr = ReceiverSession::new(session_tx);
        let bridge = SpeakerBridge::new();
        let playback_ctrl = PlaybackController::new(bridge, playback_tx);
        let queue_mgr = QueueManager::new(http, queue_tx);
        let client_comm = ClientCommunicator::new(server);

        let routing_cancel = cancel.clone();
        tasks.spawn(
            "receiver-routing",
            routing_loop(
                client_comm,
                session_mgr,
                playback_ctrl,
                queue_mgr,
                bridge_rx,
                client_rx,
                session_rx,
                playback_rx,
                queue_rx,
                routing_cancel,
            ),
        )?;

        Ok((Self { tasks, advertiser }, bridge_tx))
    }

    /// Tear down WS server, mDNS advertiser and routing task. mDNS gets a fifth of
    /// `deadline`, then the TaskGroup all of it.
    pub async fn shutdown(
        &mut self,
        deadline: Duration,
    ) -> crate::connect::runtime::ShutdownReport {
        // Stop advertising (unregister + goodbye packet) and then drive a
        // bounded shutdown of the mDNS daemon thread via `MdnsBackend`.
        // The backend is idempotent: if the daemon is already stopped,
        // it short-circuits to `AlreadyStopped`.
        use crate::connect::mdns::backend::MdnsBackend;
        if let Some(ref mut adv) = self.advertiser.take() {
            adv.stop();
            let backend = crate::connect::mdns::backend::ProdMdnsBackend::new(adv.daemon());
            let outcome = backend.shutdown(deadline / 5).await;
            crate::vprintln!("[connect::receiver] mdns shutdown: {:?}", outcome);
        }

        // TaskGroup::shutdown cancels the shared token, waits for
        // cooperating tasks, aborts stragglers, and returns a classified
        // report. Panics are visible here because the project forces
        // `panic = "unwind"` (see runtime.rs const_assert).
        let report = self.tasks.shutdown(deadline).await;
        crate::vprintln!(
            "[connect::receiver] Shutdown report: graceful={:?} aborted={:?} panicked={:?}",
            report.graceful_completed,
            report.aborted,
            report.panicked,
        );
        report
    }
}

#[allow(clippy::too_many_arguments)]
async fn routing_loop(
    mut client_comm: ClientCommunicator,
    mut session_mgr: ReceiverSession,
    mut playback_ctrl: PlaybackController,
    mut queue_mgr: QueueManager,
    mut bridge_rx: mpsc::Receiver<BridgeEvent>,
    mut client_rx: mpsc::Receiver<ClientEvent>,
    mut session_rx: mpsc::Receiver<SessionNotifyEvent>,
    mut playback_rx: mpsc::Receiver<PlaybackNotifyEvent>,
    mut queue_rx: mpsc::Receiver<QueueNotifyEvent>,
    cancel: CancellationToken,
) {
    crate::vprintln!("[connect::receiver] Routing loop started");

    // Open WS client sockets. The empty<->non-empty edge toggles the bridge idle
    // state (bridge::set_client_connected): while empty, local player events are
    // not forwarded to the receiver at all; the connect notify/broadcast chain
    // stays dormant. A socket's connect is taken before its messages and its
    // disconnect (one channel, see `ClientEvent`). The set holds exactly the
    // sockets whose disconnect is still to come. The server keeps listening and
    // stays discoverable regardless. Reset the flag here: a
    // stale value from a previous receiver session can't leak in.
    let mut connected_sockets: std::collections::HashSet<u32> = std::collections::HashSet::new();
    crate::connect::bridge::set_client_connected(false);

    // The token is raced against each turn as a whole, the arm's work included, not read only
    // between turns: an arm awaits the network (queue HTTP, up to HTTP_TIMEOUT_SECS) and the
    // notify channels this loop itself drains. A turn the token cannot interrupt outlives the
    // receiver's deadline, and the abort that follows skips the teardown below.
    loop {
        let turn = cancel.run_until_cancelled(async {
        tokio::select! {
            // WS client sockets (connect, messages, disconnect), each socket's in the order
            // it lived them. No message is taken before its socket's connect or after its
            // disconnect; none arms the bridge or a session for a socket that is gone.
            Some(event) = client_rx.recv() => {
                match event {
                    ClientEvent::Connected(socket_id) => {
                        connected_sockets.insert(socket_id);
                        crate::connect::bridge::set_client_connected(true);
                        crate::vprintln!("[connect::receiver] Client {} connected", socket_id);
                    }
                    ClientEvent::Message { socket_id: sid, message } => {
                        let command = message.get("command")
                            .and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let request_id = message.get("requestId").and_then(|v| v.as_u64());
                        crate::vprintln!("[connect::receiver] <- {} (socket {}, rid={:?})", command, sid, request_id);
                        let admission = session_mgr.admission(sid);
                        let result = match classify_command(sid, &command, message, admission) {
                            DispatchedCommand::Dropped { command } => {
                                crate::vprintln!("[connect::receiver] Dropped {} from socket {} ({:?})", command, sid, admission);
                                None
                            }
                            DispatchedCommand::Session { socket_id, command, payload } => {
                                session_mgr.handle_command(socket_id, &command, &payload).await;
                                None // Sessions have their own notification protocol
                            }
                            DispatchedCommand::Playback { command, payload } => {
                                dispatch_playback(&mut playback_ctrl, &command, &payload).await;
                                Some(RequestResult::Done)
                            }
                            DispatchedCommand::Queue { socket_id, command, payload } => {
                                Some(queue_mgr.handle_command(socket_id, &command, &payload).await)
                            }
                        };
                        if let Some(result) = result
                            && let Some(rid) = request_id
                            && let Ok(reply) = serde_json::to_value(result.answer(rid))
                        {
                            let _ = client_comm.send_to(sid, &reply).await;
                        }
                    }
                    ClientEvent::Disconnected(socket_id) => {
                        connected_sockets.remove(&socket_id);
                        crate::connect::bridge::set_client_connected(!connected_sockets.is_empty());
                        session_mgr.handle_disconnect(socket_id).await;
                    }
                }
            }

            // Session notifications -> relay to WS + handle internals
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
                            SessionInternalEvent::SessionEnded(end) => {
                                playback_ctrl.reset().await;
                                match end {
                                    // Kept for a `resumeSession`: its phone steps on through
                                    // the same queue.
                                    SessionEnd::Suspended => {}
                                    SessionEnd::Over => queue_mgr.end_session(),
                                }
                            }
                            SessionInternalEvent::SessionStarted { session_id } => {
                                crate::vprintln!("[connect::receiver] Session started: {}", session_id);
                            }
                        }
                    }
                }
            }

            // Playback notifications -> relay to WS + handle internals
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

            // Queue notifications -> relay to WS + dispatch to playback
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
                        // The mirror gets the report of what the player applied: its count is
                        // what lets the page's later levels be told apart from stale ones.
                        if let Some(level) = playback_ctrl.set_volume(level) {
                            crate::connect::ipc::post_emit_with_data(
                                "connect.receiver.volume_changed",
                                &level.report(),
                            );
                        }
                    }
                    QueueNotifyEvent::SetMute { mute } => {
                        // The mirror gets the levels the player settled on, not the command: an
                        // unmute restores a level only the player knows, and TIDAL's store would
                        // otherwise pick its own.
                        if let Some(level) = playback_ctrl.set_mute(mute) {
                            crate::connect::ipc::post_emit_with_data(
                                "connect.receiver.mute_changed",
                                &level.report(),
                            );
                        }
                    }
                    QueueNotifyEvent::QueueGenBumped { queue_gen } => {
                        playback_ctrl.set_queue_gen(queue_gen);
                    }
                }
            }

            // Bridge events: the local player's, and the loads the SDK asks for during a cast
            Some(event) = bridge_rx.recv() => {
                match event {
                    BridgeEvent::Prepared { from } => {
                        crate::vprintln!("[connect::bridge] <- Prepared ({:?})", from);
                        playback_ctrl.on_prepared(from).await;
                    }
                    BridgeEvent::StatusUpdated { ref state, from } => {
                        crate::vprintln!("[connect::bridge] <- StatusUpdated {:?} ({:?})", state, from);
                        playback_ctrl.on_status_updated(*state, from).await;
                    }
                    BridgeEvent::ProgressUpdated { progress_ms, duration_ms, from } => {
                        // Don't log each progress (too noisy); one is logged via flush.rs.
                        // When idle, bridge::forward short-circuits at the source, so
                        // no bridge events reach this loop at all.
                        playback_ctrl.on_progress_updated(progress_ms, duration_ms, from).await;
                    }
                    BridgeEvent::PlaybackCompleted { has_next_media, from } => {
                        crate::vprintln!("[connect::bridge] <- PlaybackCompleted has_next={} ({:?})", has_next_media, from);
                        let _ = has_next_media;
                        playback_ctrl.on_playback_completed(from).await;
                    }
                    BridgeEvent::PlaybackError { ref status_code, from } => {
                        crate::vprintln!("[connect::bridge] <- PlaybackError {} ({:?})", status_code, from);
                        playback_ctrl.on_playback_error(status_code, from).await;
                    }
                    BridgeEvent::DurationMeasured { duration_ms, ref track_id, from } => {
                        crate::vprintln!("[connect::bridge] <- DurationMeasured {}ms ({:?})", duration_ms, from);
                        playback_ctrl.on_duration_measured(duration_ms, track_id.as_deref(), from).await;
                    }
                    BridgeEvent::LocalLoad(load) => {
                        // Answered before anything is selected or stepped. The receiver's load of
                        // the new track mints in a later turn of this loop, so the answer reaches
                        // the renderer ahead of every event that load sends.
                        let route = queue_mgr.cast_route(&load.product_id, load.act, crate::player::current_load().1);
                        crate::ui::flush::post_player_event(route.answer(load.load_id));
                        match route {
                            CastRoute::Select { index } => {
                                crate::vprintln!("[connect::receiver] Local pick {} plays from the queue (index {})", load.product_id, index);
                                queue_mgr.select_pick(index).await;
                            }
                            CastRoute::Playing => {
                                crate::vprintln!("[connect::receiver] Local load {} is the track the cast plays", load.product_id);
                            }
                            // The answer keeps the SDK's play of the reloaded entry off the cast, and
                            // it is the page's whole reply: a step refused with no queue loaded has
                            // no one left to tell, beyond the log `step` writes.
                            CastRoute::Next => {
                                crate::vprintln!("[connect::receiver] Local next");
                                let _ = queue_mgr.step(QueueStep::Next).await;
                            }
                            CastRoute::Previous => {
                                crate::vprintln!("[connect::receiver] Local previous");
                                let _ = queue_mgr.step(QueueStep::Previous).await;
                            }
                            CastRoute::RefusePick => {
                                crate::vprintln!("[connect::receiver] Local pick {} is not the cast's to take", load.product_id);
                                // Read from what plays, not from the window: none may be loaded.
                                if let Some(media) = playback_ctrl.current_media() {
                                    crate::connect::ipc::post_emit_with_data("connect.receiver.media_changed", media);
                                }
                            }
                            CastRoute::Refuse => {
                                crate::vprintln!("[connect::receiver] Local load {} refused (act: {:?})", load.product_id, load.act);
                            }
                        }
                    }
                }
            }

            else => return false,
        }
        true
        })
        .await;
        if turn != Some(true) {
            break;
        }
    }

    // A load taken into the channel is answered even when the loop ends before serving it: the
    // SDK waits on it with no timeout. Closed first, so a later one is refused by its sender
    // instead of landing behind the drain.
    bridge_rx.close();
    while let Ok(event) = bridge_rx.try_recv() {
        if let BridgeEvent::LocalLoad(load) = event {
            crate::ui::flush::post_player_event(PlayerBridgeEvent::cast_owns_player(Some(
                load.load_id,
            )));
        }
    }

    // The phone hears the session end before its socket closes, as TIDAL's own receiver says it
    // (`notifyEndedAndClose`). Queued here, written by `shutdown`, which flushes before closing.
    if let Some(revoked) = session_mgr.revoke() {
        for socket_id in revoked.sockets {
            let _ = client_comm.send_to(socket_id, &revoked.notice).await;
        }
    }

    // A receiver switched off mid-cast gets no `SessionEnded`: the loop is cancelled before any
    // disconnect is read, and the controller would own the player for the rest of the process.
    // `reset` stops only a cast (`StopScope::Cast`), leaving local music alone.
    playback_ctrl.reset().await;

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
    let mut task = ReceiverMetadataTask::new(
        media.track_id(),
        title.to_string(),
        artist.to_string(),
        duration_secs,
    );
    post_task(ThreadId::UI, Some(&mut task));

    // Emit full MediaInfo to frontend for player bar + Redux updates
    crate::connect::ipc::post_emit_with_data("connect.receiver.media_changed", media);
}

wrap_task! {
    struct ReceiverMetadataTask {
        track_id: Option<String>,
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
                // Recorded together because the controller hands them over together. The
                // frontend echoes this track back as a `player.metadata` frame after a
                // network round trip; without the id, that frame could not tell the length
                // was measured for its own track. A frame with no length records nothing; an
                // art or title refresh says nothing about a duration already decoded, and
                // repeat-one re-announces the very track that is playing.
                if let Some(secs) = self.duration_secs {
                    state.record_measured_duration(crate::app_state::MeasuredDuration::new(
                        self.track_id.clone(),
                        secs,
                    ));
                }
                match crate::state::CURRENT_METADATA.lock() {
                    Ok(mut lock) => {
                        *lock = Some(crate::state::TrackMetadata {
                            title: self.title.clone(),
                            artist: self.artist.clone(),
                            quality: String::new(),
                            id: self.track_id.clone(),
                        });
                    }
                    Err(e) => {
                        crate::vprintln!("[connect] CURRENT_METADATA lock poisoned: {e}")
                    }
                }
            });
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/connect/receiver/routing.rs"]
mod tests;
