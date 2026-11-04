use futures_util::StreamExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use crate::connect::ws::close::{CLOSE_DEADLINE, ReadEnd, answer_close, read_to_end};
use crate::connect::ws::pending::PendingRequests;

type WsRead = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

pub(crate) enum WsClientEvent {
    Message(serde_json::Value),
    ConnectionLost,
    Error(String),
}

/// Bound on a dial (TCP, TLS, WS upgrade): without it an address nothing answers on holds the
/// caller for the OS connect timeout. Half of what a command then gets (`CMD_TIMEOUT`).
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);

/// The answer to a command already on the write queue.
pub(crate) struct PendingReply {
    request_id: u32,
    rx: tokio::sync::oneshot::Receiver<serde_json::Value>,
    pending: Arc<PendingRequests>,
}

impl PendingReply {
    pub(crate) async fn wait(self, timeout_dur: Duration) -> anyhow::Result<serde_json::Value> {
        match tokio::time::timeout(timeout_dur, self.rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(anyhow::anyhow!("Request cancelled (connection closed)")),
            Err(_) => {
                self.pending.remove(self.request_id);
                Err(anyhow::anyhow!("Request timed out"))
            }
        }
    }
}

/// Connection-scoped WebSocket TLS client (controller -> device).
/// Each instance owns its connection - reconnect = new WsClient.
///
/// Dropping the last holder closes it after what is queued is written (see `Drop`);
/// `shutdown()` is the immediate stop.
pub(crate) struct WsClient {
    write_tx: mpsc::Sender<WsMessage>,
    pending: Arc<PendingRequests>,
    connected: Arc<AtomicBool>,
    /// Cooperative cancellation for the per-connection tasks: the immediate stop. Triggered by
    /// `shutdown()` (sync), and by the read task when reading ends (the peer's Close, a dead
    /// connection) or a close's deadline passes: the writer writes nothing after it.
    cancel: CancellationToken,
    /// Cancelled when the client is dropped: the read task stops serving and reads to the end.
    closing: CancellationToken,
    /// Holds a sender of the write queue, aborted with the client: kept, the writer would never
    /// see its queue close.
    _heartbeat_task: AbortOnDropHandle<()>,
}

impl WsClient {
    /// Connect to a TIDAL Connect device. Returns after WS handshake completes.
    pub async fn connect(
        address: &str,
        port: u16,
        disable_tls: bool,
        event_tx: mpsc::Sender<WsClientEvent>,
    ) -> anyhow::Result<Self> {
        let protocol = if disable_tls { "ws" } else { "wss" };
        let url = format!("{protocol}://{address}:{port}");

        let handshake = async {
            if disable_tls {
                let (stream, _) = tokio_tungstenite::connect_async(&url).await?;
                anyhow::Ok(stream)
            } else {
                let tls_config = super::tls::tidal_client_tls_config()?;
                let connector = tokio_tungstenite::Connector::Rustls(tls_config);
                let (stream, _) = tokio_tungstenite::connect_async_tls_with_config(
                    &url,
                    None,
                    false,
                    Some(connector),
                )
                .await?;
                Ok(stream)
            }
        };
        let ws_stream = tokio::time::timeout(DIAL_TIMEOUT, handshake)
            .await
            .map_err(|_| anyhow::anyhow!("Dial timed out after {}s", DIAL_TIMEOUT.as_secs()))??;

        let (write, read) = ws_stream.split();
        let connected = Arc::new(AtomicBool::new(true));
        let pending = Arc::new(PendingRequests::new());
        let (write_tx, write_rx) = mpsc::channel::<WsMessage>(64);
        let cancel = CancellationToken::new();
        let closing = CancellationToken::new();

        // Write task: forward mpsc -> WS sink, exit on cancel or when the queue closes.
        {
            let cancel = cancel.clone();
            tokio::spawn(super::writer::run(write, write_rx, cancel));
        }

        // Pong tracker for heartbeat
        let pong_received = Arc::new(AtomicBool::new(true));

        // Read task: WS stream -> dispatch, exit on cancel or when the client is dropped.
        {
            let pending = pending.clone();
            let event_tx = event_tx.clone();
            let connected = connected.clone();
            let pong_received = pong_received.clone();
            let cancel = cancel.clone();
            let closing = closing.clone();
            tokio::spawn(async move {
                read_task(
                    read,
                    pending,
                    event_tx,
                    connected,
                    pong_received,
                    cancel,
                    closing,
                )
                .await;
            });
        }

        // Heartbeat task. The shared `connected` flag doubles as the
        // heartbeat's alive check; when `shutdown()` clears it the loop
        // exits on its next tick. Dropping the client aborts it at once.
        let heartbeat_task = {
            let write_tx = write_tx.clone();
            let event_tx = event_tx.clone();
            let connected = connected.clone();
            let pong_received = pong_received.clone();
            AbortOnDropHandle::new(tokio::spawn(async move {
                super::heartbeat::run(write_tx, pong_received, connected, move || async move {
                    crate::vprintln!("[connect::ws] Ping timeout - connection lost");
                    let _ = event_tx.send(WsClientEvent::ConnectionLost).await;
                })
                .await;
            }))
        };

        crate::vprintln!("[connect::ws] Connected to {}", url);

        Ok(Self {
            write_tx,
            pending,
            connected,
            cancel,
            closing,
            _heartbeat_task: heartbeat_task,
        })
    }

    /// Registers `command` as a request and serializes it with its requestId.
    fn prepare(&self, mut command: serde_json::Value) -> anyhow::Result<(PendingReply, WsMessage)> {
        if !self.connected.load(Ordering::Relaxed) {
            anyhow::bail!("WebSocket not connected");
        }
        let (request_id, rx) = self.pending.register();
        command["requestId"] = serde_json::Value::from(request_id);
        let reply = PendingReply {
            request_id,
            rx,
            pending: self.pending.clone(),
        };
        // A failed serialization drops `reply`, which the registry reclaims on its next pass.
        let msg = WsMessage::Text(serde_json::to_string(&command)?.into());
        Ok((reply, msg))
    }

    /// Send a command with a requestId and await the matching response (with timeout).
    pub async fn send_command(
        &self,
        command: serde_json::Value,
        timeout_dur: Duration,
    ) -> anyhow::Result<serde_json::Value> {
        let (reply, msg) = self.prepare(command)?;
        self.write_tx
            .send(msg)
            .await
            .map_err(|_| anyhow::anyhow!("WebSocket write channel closed"))?;
        reply.wait(timeout_dur).await
    }

    /// Puts a command on the write queue without waiting: the caller fixes its place on the wire
    /// in a critical section of its own and awaits the answer outside it. A full queue fails the
    /// command rather than blocking.
    pub fn queue_command(&self, command: serde_json::Value) -> anyhow::Result<PendingReply> {
        let (reply, msg) = self.prepare(command)?;
        self.try_enqueue(msg)?;
        Ok(reply)
    }

    /// Puts a message without requestId on the write queue without waiting, as `queue_command`
    /// does.
    pub fn queue_raw(&self, message: serde_json::Value) -> anyhow::Result<()> {
        self.try_enqueue(self.raw_frame(&message)?)
    }

    fn try_enqueue(&self, msg: WsMessage) -> anyhow::Result<()> {
        self.write_tx.try_send(msg).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => anyhow::anyhow!("WebSocket write queue full"),
            mpsc::error::TrySendError::Closed(_) => {
                anyhow::anyhow!("WebSocket write channel closed")
            }
        })
    }

    fn raw_frame(&self, message: &serde_json::Value) -> anyhow::Result<WsMessage> {
        if !self.connected.load(Ordering::Relaxed) {
            anyhow::bail!("WebSocket not connected");
        }
        Ok(WsMessage::Text(serde_json::to_string(message)?.into()))
    }

    /// Send a message without requestId (fire-and-forget, e.g. startSession).
    pub async fn send_raw(&self, message: serde_json::Value) -> anyhow::Result<()> {
        let msg = self.raw_frame(&message)?;
        self.write_tx
            .send(msg)
            .await
            .map_err(|_| anyhow::anyhow!("WebSocket write channel closed"))?;
        Ok(())
    }

    /// Stops the connection at once. Sync, idempotent, Arc-compatible (`&self`).
    ///
    /// Flips `connected` (which the heartbeat observes as its alive flag),
    /// cancels the shared token for `read_loop`/`writer::run` to exit their
    /// `select!` arms, and fails any pending request waiters. What the write queue still
    /// holds is lost: dropping the last holder is the close that writes it first.
    pub fn shutdown(&self) {
        self.connected.store(false, Ordering::Relaxed);
        self.cancel.cancel();
        self.pending.fail_all();
        crate::vprintln!("[connect::ws] Disconnected");
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

/// The last holder lets go: what is queued is written, then the Close. Sync and without a
/// runtime, as it runs wherever the last `Arc` falls, the way `OwnedDaemon` stops its daemon.
impl Drop for WsClient {
    fn drop(&mut self) {
        // The fields follow: with the sender and the heartbeat gone the writer runs to the end of its
        // queue and closes. The read task reads to the end, the deadline cutting what is left.
        self.closing.cancel();
        self.pending.fail_all();
    }
}

async fn read_task(
    mut read: WsRead,
    pending: Arc<PendingRequests>,
    event_tx: mpsc::Sender<WsClientEvent>,
    connected: Arc<AtomicBool>,
    pong_received: Arc<AtomicBool>,
    cancel: CancellationToken,
    closing: CancellationToken,
) {
    // `closing` preempts every await of the loop, its sends included: the events of a client
    // nobody holds are taken by no one.
    // `None`: the owner let go, and the close is ours.
    let end = tokio::select! {
        biased;
        _ = closing.cancelled() => None,
        end = read_loop(&mut read, &pending, &event_tx, &pong_received, &cancel) => Some(end),
    };
    connected.store(false, Ordering::Relaxed);
    pending.fail_all();
    let Some(end) = end else {
        // The Close the writer sends is answered by the peer's, or crosses a Close of its own that
        // reading on answers. A peer that never answers is cut at the deadline.
        let _ = tokio::time::timeout(CLOSE_DEADLINE, read_to_end(&mut read)).await;
        cancel.cancel();
        return;
    };
    // The end of reading frees a writer parked on a peer that stopped reading: after the peer's
    // Close or a dead connection nothing can be written (tungstenite `SendAfterClosing`).
    cancel.cancel();
    let _ = event_tx.send(WsClientEvent::ConnectionLost).await;
    // The reply to the peer's Close is the reader's to write, whatever became of the writer.
    answer_close(end, &mut read).await;
}

async fn read_loop(
    read: &mut WsRead,
    pending: &PendingRequests,
    event_tx: &mpsc::Sender<WsClientEvent>,
    pong_received: &AtomicBool,
    cancel: &CancellationToken,
) -> ReadEnd {
    loop {
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => return ReadEnd::Stopped,
            r = read.next() => match r {
                Some(r) => r,
                None => return ReadEnd::Stopped,
            },
        };
        match result {
            Ok(WsMessage::Text(text)) => {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                    // Check if this is a response to a pending request
                    if let Some(request_id) = json.get("requestId").and_then(|v| v.as_u64())
                        && pending.resolve(request_id as u32, json.clone())
                    {
                        continue; // Consumed by pending request
                    }
                    // Otherwise, forward as an event
                    let _ = event_tx.send(WsClientEvent::Message(json)).await;
                }
            }
            Ok(WsMessage::Pong(_)) => {
                pong_received.store(true, Ordering::Relaxed);
            }
            Ok(WsMessage::Close(_)) => return ReadEnd::PeerClosed,
            Err(e) => {
                let _ = event_tx.send(WsClientEvent::Error(e.to_string())).await;
                return ReadEnd::Stopped;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/connect/ws/client.rs"]
mod tests;
