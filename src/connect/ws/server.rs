use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::{RwLock, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

use crate::connect::runtime::TaskGroup;

pub(crate) struct IncomingMessage {
    pub socket_id: u32,
    pub message: serde_json::Value,
}

pub(crate) enum ServerEvent {
    ClientConnected(u32),
    ClientDisconnected(u32),
}

/// Per-client task state (kept out of `TaskGroup`, which needs static names).
/// `cancel` is a child of the server token that stops the read/write loops
/// cooperatively; `_read_task` is also aborted on shutdown as a backstop, since
/// a read parked in a send never observes the token.
struct ClientHandle {
    write_tx: mpsc::Sender<WsMessage>,
    cancel: CancellationToken,
    _read_task: tokio::task::JoinHandle<()>,
    _heartbeat_task: tokio::task::JoinHandle<()>,
}

/// WebSocket server for the receiver (accepts connections from mobile TIDAL clients).
pub(crate) struct WsServer {
    tasks: Arc<TaskGroup>,
    clients: Arc<RwLock<HashMap<u32, ClientHandle>>>,
}

const SERVER_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(2);

/// Cap on a single inbound WS message. Connect payloads are small (a queue
/// window is clamped to a few hundred items); this bounds the per-message
/// allocation far below tungstenite's 64 MiB default.
const MAX_WS_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

impl WsServer {
    /// Start listening on the given port.
    pub async fn start(
        port: u16,
        incoming_tx: mpsc::Sender<IncomingMessage>,
        server_event_tx: mpsc::Sender<ServerEvent>,
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", port)).await?;
        let tls_acceptor = build_tls_acceptor()?;
        let clients: Arc<RwLock<HashMap<u32, ClientHandle>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let next_socket_id = Arc::new(AtomicU32::new(1));

        let tasks = Arc::new(TaskGroup::new());
        let cancel = tasks.cancel_token();
        {
            let clients = clients.clone();
            let next_socket_id = next_socket_id.clone();
            tasks.spawn(
                "ws-server-listener",
                accept_loop(
                    listener,
                    tls_acceptor,
                    clients,
                    next_socket_id,
                    incoming_tx,
                    server_event_tx,
                    cancel,
                ),
            )?;
        }

        crate::vprintln!("[connect::ws::server] Listening on port {}", port);

        Ok(Self { tasks, clients })
    }

    /// Send a message to a specific client (unicast).
    pub async fn send_to(&self, socket_id: u32, message: &serde_json::Value) -> anyhow::Result<()> {
        let clients = self.clients.read().await;
        if let Some(client) = clients.get(&socket_id) {
            let text = serde_json::to_string(message)?;
            client
                .write_tx
                .send(WsMessage::Text(text.into()))
                .await
                .map_err(|_| anyhow::anyhow!("Client {} disconnected", socket_id))?;
            Ok(())
        } else {
            Err(anyhow::anyhow!("No client with socket_id {}", socket_id))
        }
    }

    /// Send a message to all connected clients (multicast).
    pub async fn broadcast(&self, message: &serde_json::Value) {
        let text = match serde_json::to_string(message) {
            Ok(t) => t,
            Err(_) => return,
        };
        let msg = WsMessage::Text(text.into());
        let clients = self.clients.read().await;
        for client in clients.values() {
            let _ = client.write_tx.send(msg.clone()).await;
        }
    }

    /// Shut down the server and all client connections.
    pub async fn shutdown(&mut self) {
        // Cancelling the TaskGroup token stops the accept loop's select! and,
        // as the parent of every per-client token, the read/write loops too,
        // so they close their sinks gracefully before any abort.
        let report = self.tasks.shutdown(SERVER_SHUTDOWN_DEADLINE).await;
        if !report.panicked.is_empty() {
            crate::vprintln!(
                "[connect::ws::server] Listener panicked: {:?}",
                report.panicked
            );
        }

        let mut clients = self.clients.write().await;
        for (_, client) in clients.drain() {
            client.cancel.cancel();
            client._read_task.abort();
            client._heartbeat_task.abort();
        }

        crate::vprintln!("[connect::ws::server] Shut down");
    }
}

/// Await the next inbound connection, returning `None` if `cancel` fires
/// first so the accept loop breaks instead of blocking inside `accept()`.
async fn accept_or_cancel(
    listener: &TcpListener,
    cancel: &CancellationToken,
) -> Option<std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)>> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        res = listener.accept() => Some(res),
    }
}

async fn accept_loop(
    listener: TcpListener,
    tls_acceptor: TlsAcceptor,
    clients: Arc<RwLock<HashMap<u32, ClientHandle>>>,
    next_socket_id: Arc<AtomicU32>,
    incoming_tx: mpsc::Sender<IncomingMessage>,
    server_event_tx: mpsc::Sender<ServerEvent>,
    cancel: CancellationToken,
) {
    loop {
        let (stream, addr) = match accept_or_cancel(&listener, &cancel).await {
            None => break,
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                crate::vprintln!("[connect::ws::server] Accept error: {}", e);
                continue;
            }
        };

        let tls_stream = match tls_acceptor.accept(stream).await {
            Ok(s) => s,
            Err(e) => {
                crate::vprintln!(
                    "[connect::ws::server] TLS handshake failed from {}: {}",
                    addr,
                    e
                );
                continue;
            }
        };

        let ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(MAX_WS_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_WS_MESSAGE_BYTES));
        let ws_stream =
            match tokio_tungstenite::accept_async_with_config(tls_stream, Some(ws_config)).await {
                Ok(s) => s,
                Err(e) => {
                    crate::vprintln!(
                        "[connect::ws::server] WS handshake failed from {}: {}",
                        addr,
                        e
                    );
                    continue;
                }
            };

        let socket_id = next_socket_id.fetch_add(1, Ordering::Relaxed);
        crate::vprintln!(
            "[connect::ws::server] Client {} connected from {}",
            socket_id,
            addr
        );

        let (write, read) = ws_stream.split();

        // Per-client cooperative cancellation: a child of the server token, so
        // server shutdown cancels it, and it can be cancelled directly on this
        // client's disconnect to stop its read/write loops gracefully.
        let client_cancel = cancel.child_token();

        // Write task: forward mpsc -> WS sink, exit on cancel.
        let (write_tx, write_rx) = mpsc::channel::<WsMessage>(64);
        tokio::spawn(client_write_loop(write, write_rx, client_cancel.clone()));

        // Pong tracker
        let pong_received = Arc::new(AtomicBool::new(true));
        let client_alive = Arc::new(AtomicBool::new(true));

        // Read task: WS stream -> dispatch, exit on cancel.
        let _read_task = {
            let clients = clients.clone();
            let incoming_tx = incoming_tx.clone();
            let server_event_tx = server_event_tx.clone();
            let pong_received = pong_received.clone();
            let client_alive = client_alive.clone();
            let cancel = client_cancel.clone();
            tokio::spawn(async move {
                client_read_loop(
                    socket_id,
                    read,
                    incoming_tx,
                    clients,
                    server_event_tx,
                    pong_received,
                    client_alive,
                    cancel,
                )
                .await;
            })
        };

        // Per-client heartbeat
        let _heartbeat_task = {
            let write_tx = write_tx.clone();
            let pong_received = pong_received.clone();
            let client_alive = client_alive.clone();
            let timeout_cancel = client_cancel.clone();
            tokio::spawn(async move {
                super::heartbeat::run(write_tx, pong_received, client_alive, move || async move {
                    crate::vprintln!(
                        "[connect::ws::server] Client {} ping timeout - disconnecting",
                        socket_id
                    );
                    // Tear down the silent peer: cancel the read/write loops so its
                    // tasks, socket, and map entry are reclaimed (the read-loop
                    // cleanup removes the client). Previously the timeout only logged.
                    timeout_cancel.cancel();
                })
                .await;
            })
        };

        clients.write().await.insert(
            socket_id,
            ClientHandle {
                write_tx,
                cancel: client_cancel,
                _read_task,
                _heartbeat_task,
            },
        );

        if server_event_tx
            .send(ServerEvent::ClientConnected(socket_id))
            .await
            .is_err()
        {
            crate::vprintln!(
                "[connect::ws::server] Dropped ClientConnected({}) event",
                socket_id
            );
        }
    }
}

async fn client_write_loop(
    mut write: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>,
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

async fn client_read_loop(
    socket_id: u32,
    mut read: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>,
    >,
    incoming_tx: mpsc::Sender<IncomingMessage>,
    clients: Arc<RwLock<HashMap<u32, ClientHandle>>>,
    server_event_tx: mpsc::Sender<ServerEvent>,
    pong_received: Arc<AtomicBool>,
    client_alive: Arc<AtomicBool>,
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
                    let dispatched = incoming_tx
                        .send(IncomingMessage {
                            socket_id,
                            message: json,
                        })
                        .await;
                    if dispatched.is_err() {
                        crate::vprintln!(
                            "[connect::ws::server] Dropped inbound message from {} (routing loop gone)",
                            socket_id
                        );
                    }
                }
            }
            Ok(WsMessage::Pong(_)) => {
                pong_received.store(true, Ordering::Relaxed);
            }
            Ok(WsMessage::Close(_)) => break,
            Err(_) => break,
            _ => {}
        }
    }

    // Client disconnected - cleanup
    client_alive.store(false, Ordering::Relaxed);
    let mut clients_guard = clients.write().await;
    if let Some(client) = clients_guard.remove(&socket_id) {
        client.cancel.cancel();
        client._heartbeat_task.abort();
    }
    drop(clients_guard);

    crate::vprintln!("[connect::ws::server] Client {} disconnected", socket_id);
    if server_event_tx
        .send(ServerEvent::ClientDisconnected(socket_id))
        .await
        .is_err()
    {
        crate::vprintln!(
            "[connect::ws::server] Dropped ClientDisconnected({}) event",
            socket_id
        );
    }
}

/// Build a TLS acceptor using the embedded TIDAL Connect server cert+key.
fn build_tls_acceptor() -> anyhow::Result<TlsAcceptor> {
    use rustls::ServerConfig;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    static CERT_PEM: &[u8] = include_bytes!("certs/tidal_server_cert.pem");
    static KEY_PEM: &[u8] = include_bytes!("certs/tidal_server_key.pem");

    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut &CERT_PEM[..]).collect::<Result<Vec<_>, _>>()?;
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut &KEY_PEM[..])
        .map_err(|e| anyhow::anyhow!("Failed to parse private key: {}", e))?
        .ok_or_else(|| anyhow::anyhow!("No private key found in PEM"))?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The accept must observe a cancelled token instead of blocking forever in
    // accept() when no connection arrives. Driven at the helper so it needs no
    // TLS/connection setup.
    #[tokio::test]
    async fn accept_or_cancel_yields_none_when_cancelled() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result =
            tokio::time::timeout(Duration::from_secs(1), accept_or_cancel(&listener, &cancel))
                .await
                .expect("accept_or_cancel did not observe cancellation");
        assert!(result.is_none(), "cancelled accept must yield None");
    }
}
