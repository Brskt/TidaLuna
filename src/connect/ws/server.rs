use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{RwLock, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::connect::types::consts;

pub(crate) struct IncomingMessage {
    pub socket_id: u32,
    pub message: serde_json::Value,
}

pub(crate) enum ServerEvent {
    ClientConnected(u32),
    ClientDisconnected(u32),
}

struct ClientHandle {
    write_tx: mpsc::Sender<WsMessage>,
    _read_task: tokio::task::JoinHandle<()>,
    _heartbeat_task: tokio::task::JoinHandle<()>,
}

/// WebSocket server for the receiver (accepts connections from mobile TIDAL clients).
pub(crate) struct WsServer {
    listener_task: Option<tokio::task::JoinHandle<()>>,
    clients: Arc<RwLock<HashMap<u32, ClientHandle>>>,
    running: Arc<AtomicBool>,
}

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
        let running = Arc::new(AtomicBool::new(true));

        let listener_task = {
            let clients = clients.clone();
            let next_socket_id = next_socket_id.clone();
            let running = running.clone();
            tokio::spawn(async move {
                accept_loop(
                    listener,
                    tls_acceptor,
                    clients,
                    next_socket_id,
                    incoming_tx,
                    server_event_tx,
                    running,
                )
                .await;
            })
        };

        crate::vprintln!("[connect::ws::server] Listening on port {}", port);

        Ok(Self {
            listener_task: Some(listener_task),
            clients,
            running,
        })
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
        self.running.store(false, Ordering::Relaxed);

        if let Some(task) = self.listener_task.take() {
            task.abort();
        }

        let mut clients = self.clients.write().await;
        for (_, client) in clients.drain() {
            client._read_task.abort();
            client._heartbeat_task.abort();
        }

        crate::vprintln!("[connect::ws::server] Shut down");
    }
}

async fn accept_loop(
    listener: TcpListener,
    tls_acceptor: TlsAcceptor,
    clients: Arc<RwLock<HashMap<u32, ClientHandle>>>,
    next_socket_id: Arc<AtomicU32>,
    incoming_tx: mpsc::Sender<IncomingMessage>,
    server_event_tx: mpsc::Sender<ServerEvent>,
    running: Arc<AtomicBool>,
) {
    while running.load(Ordering::Relaxed) {
        let (stream, addr) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
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

        let ws_stream = match tokio_tungstenite::accept_async(tls_stream).await {
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

        // Write task
        let (write_tx, write_rx) = mpsc::channel::<WsMessage>(64);
        let _write_task = tokio::spawn(client_write_loop(write, write_rx));

        // Pong tracker
        let pong_received = Arc::new(AtomicBool::new(true));
        let client_alive = Arc::new(AtomicBool::new(true));

        // Read task
        let _read_task = {
            let clients = clients.clone();
            let incoming_tx = incoming_tx.clone();
            let server_event_tx = server_event_tx.clone();
            let pong_received = pong_received.clone();
            let client_alive = client_alive.clone();
            tokio::spawn(async move {
                client_read_loop(
                    socket_id,
                    read,
                    incoming_tx,
                    clients,
                    server_event_tx,
                    pong_received,
                    client_alive,
                )
                .await;
            })
        };

        // Per-client heartbeat
        let _heartbeat_task = {
            let write_tx = write_tx.clone();
            let pong_received = pong_received.clone();
            let client_alive = client_alive.clone();
            tokio::spawn(async move {
                client_heartbeat(socket_id, write_tx, pong_received, client_alive).await;
            })
        };

        clients.write().await.insert(
            socket_id,
            ClientHandle {
                write_tx,
                _read_task,
                _heartbeat_task,
            },
        );

        let _ = server_event_tx
            .send(ServerEvent::ClientConnected(socket_id))
            .await;
    }
}

async fn client_write_loop(
    mut write: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>,
        WsMessage,
    >,
    mut rx: mpsc::Receiver<WsMessage>,
) {
    while let Some(msg) = rx.recv().await {
        if write.send(msg).await.is_err() {
            break;
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
) {
    while let Some(result) = read.next().await {
        match result {
            Ok(WsMessage::Text(text)) => {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                    let _ = incoming_tx
                        .send(IncomingMessage {
                            socket_id,
                            message: json,
                        })
                        .await;
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
        client._heartbeat_task.abort();
    }
    drop(clients_guard);

    crate::vprintln!("[connect::ws::server] Client {} disconnected", socket_id);
    let _ = server_event_tx
        .send(ServerEvent::ClientDisconnected(socket_id))
        .await;
}

async fn client_heartbeat(
    socket_id: u32,
    write_tx: mpsc::Sender<WsMessage>,
    pong_received: Arc<AtomicBool>,
    client_alive: Arc<AtomicBool>,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(consts::PING_INTERVAL_MS));

    loop {
        interval.tick().await;
        if !client_alive.load(Ordering::Relaxed) {
            break;
        }

        pong_received.store(false, Ordering::Relaxed);
        if write_tx.send(WsMessage::Ping(vec![].into())).await.is_err() {
            break;
        }

        let pong_wait = Duration::from_millis(consts::PING_TIMEOUT_MS - consts::PING_INTERVAL_MS);
        tokio::time::sleep(pong_wait).await;

        if !pong_received.load(Ordering::Relaxed) && client_alive.load(Ordering::Relaxed) {
            crate::vprintln!(
                "[connect::ws::server] Client {} ping timeout - disconnecting",
                socket_id
            );
            client_alive.store(false, Ordering::Relaxed);
            // Drop write_tx to close the channel - this propagates through
            // client_write_loop → TCP close → client_read_loop → ClientDisconnected
            drop(write_tx);
            return;
        }
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
