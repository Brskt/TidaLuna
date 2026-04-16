use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

use crate::connect::ws::pending::PendingRequests;

pub(crate) enum WsClientEvent {
    Message(serde_json::Value),
    ConnectionLost,
    Error(String),
}

/// Connection-scoped WebSocket TLS client (controller → device).
/// Each instance owns its connection - reconnect = new WsClient.
pub(crate) struct WsClient {
    write_tx: mpsc::Sender<WsMessage>,
    pending: Arc<PendingRequests>,
    connected: Arc<AtomicBool>,
    /// Cooperative cancellation for the three per-connection tasks.
    /// Triggered from `shutdown()` (sync) so the tasks can flush and close
    /// the WS stream gracefully instead of being aborted mid-frame.
    cancel: CancellationToken,
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

        let ws_stream = if disable_tls {
            let (stream, _) = tokio_tungstenite::connect_async(&url).await?;
            stream
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
            stream
        };

        let (write, read) = ws_stream.split();
        let connected = Arc::new(AtomicBool::new(true));
        let pending = Arc::new(PendingRequests::new());
        let (write_tx, write_rx) = mpsc::channel::<WsMessage>(64);
        let cancel = CancellationToken::new();

        // Write task: forward mpsc → WS sink, exit on cancel.
        {
            let cancel = cancel.clone();
            tokio::spawn(write_loop(write, write_rx, cancel));
        }

        // Pong tracker for heartbeat
        let pong_received = Arc::new(AtomicBool::new(true));

        // Read task: WS stream → dispatch, exit on cancel.
        {
            let pending = pending.clone();
            let event_tx = event_tx.clone();
            let connected = connected.clone();
            let pong_received = pong_received.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                read_loop(read, pending, event_tx, connected, pong_received, cancel).await;
            });
        }

        // Heartbeat task. The shared `connected` flag doubles as the
        // heartbeat's alive check; when `shutdown()` clears it the loop
        // exits on its next tick.
        {
            let write_tx = write_tx.clone();
            let event_tx = event_tx.clone();
            let connected = connected.clone();
            let pong_received = pong_received.clone();
            tokio::spawn(async move {
                super::heartbeat::run(write_tx, pong_received, connected, move || async move {
                    crate::vprintln!("[connect::ws] Ping timeout - connection lost");
                    let _ = event_tx.send(WsClientEvent::ConnectionLost).await;
                })
                .await;
            });
        }

        crate::vprintln!("[connect::ws] Connected to {}", url);

        Ok(Self {
            write_tx,
            pending,
            connected,
            cancel,
        })
    }

    /// Send a command with a requestId and await the matching response (with timeout).
    pub async fn send_command(
        &self,
        mut command: serde_json::Value,
        timeout_dur: Duration,
    ) -> anyhow::Result<serde_json::Value> {
        if !self.connected.load(Ordering::Relaxed) {
            anyhow::bail!("WebSocket not connected");
        }

        let (request_id, rx) = self.pending.register();
        command["requestId"] = serde_json::Value::from(request_id);

        let msg = WsMessage::Text(serde_json::to_string(&command)?.into());
        self.write_tx
            .send(msg)
            .await
            .map_err(|_| anyhow::anyhow!("WebSocket write channel closed"))?;

        match tokio::time::timeout(timeout_dur, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(anyhow::anyhow!("Request cancelled (connection closed)")),
            Err(_) => {
                self.pending.remove(request_id);
                Err(anyhow::anyhow!("Request timed out"))
            }
        }
    }

    /// Send a message without requestId (fire-and-forget, e.g. startSession).
    pub async fn send_raw(&self, message: serde_json::Value) -> anyhow::Result<()> {
        if !self.connected.load(Ordering::Relaxed) {
            anyhow::bail!("WebSocket not connected");
        }
        let msg = WsMessage::Text(serde_json::to_string(&message)?.into());
        self.write_tx
            .send(msg)
            .await
            .map_err(|_| anyhow::anyhow!("WebSocket write channel closed"))?;
        Ok(())
    }

    /// Shutdown the connection. Sync, idempotent, Arc-compatible (`&self`).
    ///
    /// Flips `connected` (which the heartbeat observes as its alive flag),
    /// cancels the shared token so `read_loop`/`write_loop` exit their
    /// `select!` arms, and fails any pending request waiters. The three
    /// tasks unwind gracefully on their own; we no longer `abort()` them
    /// so the WS stream gets a chance to close properly.
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

async fn write_loop(
    mut write: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        WsMessage,
    >,
    mut rx: mpsc::Receiver<WsMessage>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            msg = rx.recv() => match msg {
                Some(m) => {
                    if write.send(m).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
        }
    }
    let _ = write.close().await;
}

async fn read_loop(
    mut read: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    pending: Arc<PendingRequests>,
    event_tx: mpsc::Sender<WsClientEvent>,
    connected: Arc<AtomicBool>,
    pong_received: Arc<AtomicBool>,
    cancel: CancellationToken,
) {
    loop {
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            r = read.next() => match r {
                Some(r) => r,
                None => break,
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
            Ok(WsMessage::Close(_)) => {
                break;
            }
            Err(e) => {
                let _ = event_tx.send(WsClientEvent::Error(e.to_string())).await;
                break;
            }
            _ => {}
        }
    }

    connected.store(false, Ordering::Relaxed);
    pending.fail_all();
    let _ = event_tx.send(WsClientEvent::ConnectionLost).await;
}
