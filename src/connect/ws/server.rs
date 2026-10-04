use futures_util::StreamExt;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::{RwLock, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::{CancellationToken, DropGuard};
use tokio_util::task::AbortOnDropHandle;

use crate::connect::runtime::TaskGroup;
use crate::connect::ws::writer;

/// A client socket's life as the routing loop hears it, every socket on one channel. `Connected`
/// is queued before the socket's read loop exists and `Disconnected` is that loop's last send, so
/// a socket's events are taken in the order they happened: connect, messages, disconnect.
pub(crate) enum ClientEvent {
    Connected(u32),
    Message {
        socket_id: u32,
        message: serde_json::Value,
    },
    Disconnected(u32),
}

/// Per-client task state (kept out of `TaskGroup`, which needs static names).
///
/// Dropping the entry ends the client: its token is cancelled, which the read loop leaves on
/// to report the disconnect, and the heartbeat and the writer are aborted. `shutdown` first
/// cancels `closing` for every registered client, whose read loop observes it even parked in
/// a send.
struct ClientHandle {
    write_tx: mpsc::Sender<WsMessage>,
    cancel: DropGuard,
    /// Cancelled by `shutdown`: the read loop stops serving, then reads up to the peer's Close.
    closing: CancellationToken,
    _read_task: tokio::task::JoinHandle<()>,
    _heartbeat_task: AbortOnDropHandle<()>,
    /// Awaited by `shutdown`, up to its deadline, for what was queued to be written.
    write_task: AbortOnDropHandle<()>,
}

/// WebSocket server for the receiver (accepts connections from mobile TIDAL clients).
pub(crate) struct WsServer {
    tasks: Arc<TaskGroup>,
    clients: Arc<RwLock<HashMap<u32, ClientHandle>>>,
}

const SERVER_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(2);

/// Messages a client may leave untaken before it counts as no longer reading.
const CLIENT_QUEUE: usize = 64;

/// Cap on a single inbound WS message. Connect payloads are small (a queue
/// window is clamped to a few hundred items); this bounds the per-message
/// allocation far below tungstenite's 64 MiB default.
const MAX_WS_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

impl WsServer {
    /// Start listening on the given port.
    pub async fn start(port: u16, events_tx: mpsc::Sender<ClientEvent>) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", port)).await?;
        let server = Self::serve(listener, events_tx)?;
        crate::vprintln!("[connect::ws::server] Listening on port {}", port);
        Ok(server)
    }

    /// Serves on a listener already bound. Split from `start` so a test can bind port 0 and read
    /// the port it got, where probing for a free one races every other bind. Generic over how the
    /// listener is held so that test can also hand over one whose drop it observes.
    fn serve<L>(listener: L, events_tx: mpsc::Sender<ClientEvent>) -> anyhow::Result<Self>
    where
        L: Borrow<TcpListener> + Send + Sync + 'static,
    {
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
                    events_tx,
                    cancel,
                ),
            )?;
        }

        Ok(Self { tasks, clients })
    }

    /// Send a message to a specific client (unicast).
    ///
    /// Never waits on the peer. A client whose queue is full has left `CLIENT_QUEUE` messages
    /// untaken: it stopped reading, and it is dropped along with the message.
    pub async fn send_to(&self, socket_id: u32, message: &serde_json::Value) -> anyhow::Result<()> {
        let text = serde_json::to_string(message)?;
        let queued = {
            let clients = self.clients.read().await;
            let Some(client) = clients.get(&socket_id) else {
                return Err(anyhow::anyhow!("No client with socket_id {}", socket_id));
            };
            client.write_tx.try_send(WsMessage::Text(text.into()))
        };
        if let Err(e) = queued {
            self.evict(&[socket_id]).await;
            return Err(anyhow::anyhow!("Client {} dropped: {}", socket_id, e));
        }
        Ok(())
    }

    /// Send a message to all connected clients (multicast), dropping those that stopped reading.
    pub async fn broadcast(&self, message: &serde_json::Value) {
        let text = match serde_json::to_string(message) {
            Ok(t) => t,
            Err(_) => return,
        };
        let msg = WsMessage::Text(text.into());
        let stalled: Vec<u32> = {
            let clients = self.clients.read().await;
            clients
                .iter()
                .filter(|(_, client)| client.write_tx.try_send(msg.clone()).is_err())
                .map(|(socket_id, _)| *socket_id)
                .collect()
        };
        if !stalled.is_empty() {
            self.evict(&stalled).await;
        }
    }

    /// Disconnect clients that no longer take their queue: dropping the entry ends them, and their
    /// read loop, left on the cancelled token, reports the disconnect.
    async fn evict(&self, socket_ids: &[u32]) {
        let mut clients = self.clients.write().await;
        for socket_id in socket_ids {
            if clients.remove(socket_id).is_some() {
                crate::vprintln!(
                    "[connect::ws::server] Client {} stopped reading: disconnecting",
                    socket_id
                );
            }
        }
    }

    /// Shut down the server and all client connections.
    ///
    /// What was queued before the call is written before the close frame: the writers drain first.
    /// Cancelled, their biased select would drop every message not yet taken, the session's end
    /// among them. A socket is released once the peer's Close is read: released on bytes still
    /// unread, it would be reset, and the reset discards what the peer has yet to receive.
    pub async fn shutdown(&mut self) {
        // Each writer keeps its token uncancelled until it is done: cancelled, it would stop
        // taking its queue.
        let mut ending: Vec<(DropGuard, AbortOnDropHandle<()>, AbortOnDropHandle<()>)> = {
            let mut clients = self.clients.write().await;
            clients
                .drain()
                .map(|(_, client)| {
                    // The heartbeat, dropped here, holds the only other sender. With it and the
                    // handle's own gone, the writer runs to the end of its queue and closes.
                    client.closing.cancel();
                    (
                        client.cancel,
                        client.write_task,
                        AbortOnDropHandle::new(client._read_task),
                    )
                })
                .collect()
        };
        let _ = tokio::time::timeout(SERVER_SHUTDOWN_DEADLINE, async {
            for (_, writer, reader) in ending.iter_mut() {
                let _ = writer.await;
                let _ = reader.await;
            }
        })
        .await;
        // A peer that stopped reading leaves its writer parked in a send, and one that never
        // answers the Close leaves its reader waiting: the drop aborts both.
        drop(ending);

        // Cancelling the TaskGroup token stops the accept loop's select! and,
        // as the parent of every per-client token, the loops of any client
        // accepted during the flush; the drain below reclaims those.
        let report = self.tasks.shutdown(SERVER_SHUTDOWN_DEADLINE).await;
        if !report.panicked.is_empty() {
            crate::vprintln!(
                "[connect::ws::server] Listener panicked: {:?}",
                report.panicked
            );
        }

        let mut clients = self.clients.write().await;
        for (_, client) in clients.drain() {
            client._read_task.abort();
        }

        crate::vprintln!("[connect::ws::server] Shut down");
    }
}

/// Await the next inbound connection, returning `None` if `cancel` fires
/// first: the accept loop breaks instead of blocking inside `accept()`.
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

async fn accept_loop<L: Borrow<TcpListener>>(
    listener: L,
    tls_acceptor: TlsAcceptor,
    clients: Arc<RwLock<HashMap<u32, ClientHandle>>>,
    next_socket_id: Arc<AtomicU32>,
    events_tx: mpsc::Sender<ClientEvent>,
    cancel: CancellationToken,
) {
    loop {
        let (stream, addr) = match accept_or_cancel(listener.borrow(), &cancel).await {
            None => break,
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                crate::vprintln!("[connect::ws::server] Accept error: {}", e);
                continue;
            }
        };
        // Without it, a phone that never reaches this socket and one stuck in the handshakes
        // below leave the same silence.
        crate::vprintln!("[connect::ws::server] TCP connection from {}", addr);

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
        crate::vprintln2!("[connect::ws::server] TLS handshake done with {}", addr);

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

        // Queued before the read loop exists, so it reaches the routing loop ahead of everything
        // that loop sends for this socket. The token is raced: the routing loop, in its epilogue,
        // takes nothing more, and a full channel would hold this loop past the shutdown deadline.
        let connected = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            sent = events_tx.send(ClientEvent::Connected(socket_id)) => sent,
        };
        if connected.is_err() {
            crate::vprintln!(
                "[connect::ws::server] Dropped client {} (routing loop gone)",
                socket_id
            );
            continue;
        }

        let (write, read) = ws_stream.split();

        // Per-client cooperative cancellation: a child of the server token, so
        // server shutdown cancels it, and it can be cancelled directly on this
        // client's disconnect to stop its read/write loops gracefully.
        let client_cancel = cancel.child_token();
        let closing = CancellationToken::new();

        // Write task: forward mpsc -> WS sink, exit on cancel.
        let (write_tx, write_rx) = mpsc::channel::<WsMessage>(CLIENT_QUEUE);
        let write_task = AbortOnDropHandle::new(tokio::spawn(writer::run(
            write,
            write_rx,
            client_cancel.clone(),
        )));

        // Pong tracker
        let pong_received = Arc::new(AtomicBool::new(true));
        let client_alive = Arc::new(AtomicBool::new(true));

        // Held from before the read task exists until its entry is in. The read loop's cleanup
        // takes the same lock and always finds the entry it removes: a peer gone at once leaves
        // no entry behind, its writer and socket held by nobody's read loop.
        let mut registry = clients.write().await;

        // Read task: WS stream -> dispatch, exit on cancel.
        let _read_task = {
            let clients = clients.clone();
            let events_tx = events_tx.clone();
            let pong_received = pong_received.clone();
            let client_alive = client_alive.clone();
            let cancel = client_cancel.clone();
            let closing = closing.clone();
            tokio::spawn(async move {
                let mut read = read;
                // `closing` preempts every await of the loop, its sends included: the routing
                // loop, in its epilogue by then, takes no event.
                let closed = tokio::select! {
                    biased;
                    _ = closing.cancelled() => true,
                    _ = client_read_loop(
                        socket_id,
                        &mut read,
                        events_tx,
                        clients,
                        pong_received,
                        client_alive,
                        cancel,
                    ) => false,
                };
                if closed {
                    read_to_close(&mut read).await;
                }
            })
        };

        // Per-client heartbeat
        let _heartbeat_task = {
            let write_tx = write_tx.clone();
            let pong_received = pong_received.clone();
            let client_alive = client_alive.clone();
            let timeout_cancel = client_cancel.clone();
            AbortOnDropHandle::new(tokio::spawn(async move {
                super::heartbeat::run(write_tx, pong_received, client_alive, move || async move {
                    crate::vprintln!(
                        "[connect::ws::server] Client {} ping timeout - disconnecting",
                        socket_id
                    );
                    // The read loop leaves on the token and removes the entry, whose drop
                    // ends the writer and this heartbeat.
                    timeout_cancel.cancel();
                })
                .await;
            }))
        };

        registry.insert(
            socket_id,
            ClientHandle {
                write_tx,
                cancel: client_cancel.drop_guard(),
                closing,
                _read_task,
                _heartbeat_task,
                write_task,
            },
        );
        drop(registry);
    }
}

async fn client_read_loop(
    socket_id: u32,
    read: &mut futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>,
    >,
    events_tx: mpsc::Sender<ClientEvent>,
    clients: Arc<RwLock<HashMap<u32, ClientHandle>>>,
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
                    let dispatched = events_tx
                        .send(ClientEvent::Message {
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
    // Dropping the removed entry ends the heartbeat and the writer.
    clients_guard.remove(&socket_id);
    drop(clients_guard);

    crate::vprintln!("[connect::ws::server] Client {} disconnected", socket_id);
    // Every end of the socket but `closing`'s passes here: the peer's, and the server's
    // (heartbeat verdict, eviction, shutdown) through its token.
    if events_tx
        .send(ClientEvent::Disconnected(socket_id))
        .await
        .is_err()
    {
        crate::vprintln!(
            "[connect::ws::server] Dropped Disconnected({}) event",
            socket_id
        );
    }
}

/// Reads and drops what the peer still sends, up to its Close: the answer to ours, and the last
/// frame it sends. Released before it, a socket holding a Pong or a command not yet read would
/// be reset, and the reset discards what the peer has yet to receive, our Close included.
async fn read_to_close<S, E>(read: &mut S)
where
    S: futures_util::Stream<Item = Result<WsMessage, E>> + Unpin,
{
    while let Some(Ok(message)) = read.next().await {
        if let WsMessage::Close(_) = message {
            break;
        }
    }
}

/// Build a TLS acceptor using the embedded TIDAL Connect server cert+key.
#[cfg(has_connect_server_key)]
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

/// Stand-in for a build the server key never reached, `include_bytes!` having no way to
/// take a file that is absent. `ConnectReceiver::start` gives up on this error before it
/// reaches the mDNS advertiser, leaving such a build to announce no receiver at all.
#[cfg(not(has_connect_server_key))]
fn build_tls_acceptor() -> anyhow::Result<TlsAcceptor> {
    anyhow::bail!("no TIDAL Connect server key was bundled at build time")
}

/// The server as `start` builds it, the TLS accept aside, with one client (`socket_id`) whose
/// socket is the returned wire. The returned sender lets a test fill the queue; kept, the
/// writer never sees its queue close. The read task stands in for a peer that answers the Close
/// at once: it ends on `closing`.
#[cfg(test)]
impl WsServer {
    pub(crate) async fn with_client_for_test(
        socket_id: u32,
        wire_capacity: usize,
    ) -> (
        Self,
        ListenerProbe,
        mpsc::Receiver<WsMessage>,
        mpsc::Sender<WsMessage>,
    ) {
        let tasks = Arc::new(TaskGroup::new());
        let cancel = tasks.cancel_token();
        let listener = spawn_test_listener(&tasks, cancel.clone()).await;
        let client_cancel = cancel.child_token();
        let (wire_tx, wire_rx) = mpsc::channel::<WsMessage>(wire_capacity);
        let (write_tx, write_rx) = mpsc::channel::<WsMessage>(CLIENT_QUEUE);
        let write_task = tokio::spawn(writer::run(
            tokio_util::sync::PollSender::new(wire_tx),
            write_rx,
            client_cancel.clone(),
        ));
        let heartbeat_tx = write_tx.clone();
        let filler = write_tx.clone();
        let _heartbeat_task = tokio::spawn(async move {
            let _held = heartbeat_tx;
            std::future::pending::<()>().await;
        });
        let closing = CancellationToken::new();
        let clients = Arc::new(RwLock::new(HashMap::from([(
            socket_id,
            ClientHandle {
                write_tx,
                cancel: client_cancel.drop_guard(),
                closing: closing.clone(),
                _read_task: tokio::spawn(async move { closing.cancelled().await }),
                _heartbeat_task: AbortOnDropHandle::new(_heartbeat_task),
                write_task: AbortOnDropHandle::new(write_task),
            },
        )])));
        (Self { tasks, clients }, listener, wire_rx, filler)
    }
}

/// Reads whether a test listener is closed. Rebinding its port instead races every socket the
/// kernel hands that ephemeral port to once it is free, another test's listener among them.
#[cfg(test)]
pub(crate) struct ListenerProbe(std::sync::Weak<()>);

#[cfg(test)]
impl ListenerProbe {
    pub(crate) fn closed(&self) -> bool {
        self.0.strong_count() == 0
    }
}

/// A listener on a loopback port the kernel picked, with the marker its probe reads. Fields drop
/// in order: once the probe reads closed, the listener is gone, however its holder ended.
#[cfg(test)]
pub(crate) struct ProbedListener {
    listener: TcpListener,
    _alive: Arc<()>,
}

#[cfg(test)]
impl ProbedListener {
    pub(crate) async fn bind() -> (Self, ListenerProbe) {
        let alive = Arc::new(());
        let probe = ListenerProbe(Arc::downgrade(&alive));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        (
            Self {
                listener,
                _alive: alive,
            },
            probe,
        )
    }

    pub(crate) fn port(&self) -> u16 {
        self.listener.local_addr().unwrap().port()
    }
}

#[cfg(test)]
impl Borrow<TcpListener> for ProbedListener {
    fn borrow(&self) -> &TcpListener {
        &self.listener
    }
}

/// The listener as `start` runs it, in a task of `tasks` that leaves on `cancel`.
#[cfg(test)]
pub(crate) async fn spawn_test_listener(
    tasks: &TaskGroup,
    cancel: CancellationToken,
) -> ListenerProbe {
    let (held, probe) = ProbedListener::bind().await;
    tasks
        .spawn("ws-server-listener", async move {
            while accept_or_cancel(held.borrow(), &cancel).await.is_some() {}
        })
        .unwrap();
    probe
}

#[cfg(test)]
#[path = "../../../tests/unit/connect/ws/server.rs"]
mod tests;
