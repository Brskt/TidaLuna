//! Tests for `src/connect/ws/server.rs`, attached to it by `#[path]`.

use super::*;
use crate::connect::runtime::ShutdownReport;
use futures_util::SinkExt;

// The accept must observe a cancelled token instead of blocking forever in
// accept() when no connection arrives. Driven at the helper: it needs no
// TLS/connection setup.
#[tokio::test]
async fn accept_or_cancel_yields_none_when_cancelled() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(1), accept_or_cancel(&listener, &cancel))
        .await
        .expect("accept_or_cancel did not observe cancellation");
    assert!(result.is_none(), "cancelled accept must yield None");
}

/// The notice is queued right before `shutdown`, before the writer has taken it.
#[tokio::test]
async fn a_shutdown_writes_what_was_queued_before_it_closes() {
    let (mut server, _listener, mut wire, filler) = WsServer::with_client_for_test(1, 8).await;
    // Kept, it would hold the queue open: the writer would leave on the shutdown deadline's
    // abort, never on its own close, and the wire would read closed all the same.
    drop(filler);

    let notice = serde_json::json!({ "command": "notifySessionEnded" });
    server
        .send_to(1, &notice)
        .await
        .expect("the client is known");
    server.shutdown().await;

    let written = tokio::time::timeout(Duration::from_secs(1), wire.recv())
        .await
        .expect("the writer finished within the shutdown");
    assert_eq!(
        written,
        Some(WsMessage::Text(notice.to_string().into())),
        "the notice queued before the shutdown reaches the wire"
    );
    assert_eq!(wire.recv().await, None, "then the socket is closed");
}

/// Longest any enqueue may keep its caller waiting: the flush budget the shutdown grants, twice.
const ENQUEUE_BOUND: Duration = Duration::from_secs(4);

/// One ping interval, plus a margin: the longest a heartbeat may wait on its own Ping.
const HEARTBEAT_BOUND: Duration =
    Duration::from_millis(crate::connect::consts::PING_INTERVAL_MS + 1_000);

/// A client whose peer stopped reading. The wire holds one message and is never read, so the
/// writer parks in `write.send` on the next one, as it does on a full TCP send buffer. The queue
/// is then filled to capacity, as broadcasts and pings fill it while the writer is parked.
async fn stalled_client(
    cancel: CancellationToken,
) -> (
    ClientHandle,
    mpsc::Receiver<WsMessage>,
    mpsc::Sender<WsMessage>,
) {
    let (wire_tx, wire_rx) = mpsc::channel::<WsMessage>(1);
    let (write_tx, write_rx) = mpsc::channel::<WsMessage>(CLIENT_QUEUE);
    let write_task = tokio::spawn(writer::run(
        tokio_util::sync::PollSender::new(wire_tx),
        write_rx,
        cancel.clone(),
    ));
    let filler = write_tx.clone();
    loop {
        if filler.try_send(WsMessage::Ping(vec![].into())).is_err() {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if filler.try_send(WsMessage::Ping(vec![].into())).is_err() {
                break;
            }
        }
    }
    let heartbeat_tx = write_tx.clone();
    let _heartbeat_task = tokio::spawn(async move {
        let _held = heartbeat_tx;
        std::future::pending::<()>().await;
    });
    (
        ClientHandle {
            write_tx,
            cancel: cancel.drop_guard(),
            closing: CancellationToken::new(),
            _read_task: tokio::spawn(std::future::pending()),
            _heartbeat_task: AbortOnDropHandle::new(_heartbeat_task),
            write_task: AbortOnDropHandle::new(write_task),
        },
        wire_rx,
        filler,
    )
}

/// The server as `WsServer::start` builds it, the TLS accept aside: the listener lives in a
/// task of the server's group and leaves on its token.
async fn server_with_stalled_client() -> (
    WsServer,
    ListenerProbe,
    mpsc::Receiver<WsMessage>,
    mpsc::Sender<WsMessage>,
) {
    let tasks = Arc::new(TaskGroup::new());
    let cancel = tasks.cancel_token();
    let listener = spawn_test_listener(&tasks, cancel.clone()).await;
    let (client, wire, filler) = stalled_client(cancel.child_token()).await;
    let clients = Arc::new(RwLock::new(HashMap::from([(1, client)])));
    (WsServer { tasks, clients }, listener, wire, filler)
}

#[tokio::test]
async fn sending_to_a_peer_that_stopped_reading_returns_and_leaves_the_map_free() {
    let (server, _listener, _wire, _filler) = server_with_stalled_client().await;
    let notice = serde_json::json!({ "command": "notifyPlayerStatusChanged" });
    let sent = tokio::time::timeout(ENQUEUE_BOUND, server.send_to(1, &notice)).await;
    assert!(
        sent.is_ok(),
        "send_to parked on a peer that stopped reading"
    );
    let broadcast = tokio::time::timeout(ENQUEUE_BOUND, server.broadcast(&notice)).await;
    assert!(
        broadcast.is_ok(),
        "broadcast parked on a peer that stopped reading"
    );
    let cleanup = tokio::time::timeout(Duration::from_secs(1), server.clients.write()).await;
    assert!(cleanup.is_ok(), "the read-loop cleanup cannot take the map");
}

#[tokio::test]
async fn a_send_in_flight_leaves_the_map_to_the_cleanup() {
    let (server, _listener, _wire, _filler) = server_with_stalled_client().await;
    let server = Arc::new(server);
    let sender = {
        let server = server.clone();
        tokio::spawn(async move {
            let notice = serde_json::json!({ "command": "notifyMediaChanged" });
            let _ = server.send_to(1, &notice).await;
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let cleanup = tokio::time::timeout(Duration::from_secs(1), server.clients.write()).await;
    assert!(
        cleanup.is_ok(),
        "clients.write() blocked while a send_to is in flight"
    );
    drop(cleanup);
    sender.abort();
}

/// The routing epilogue on a receiver switched off while a phone stopped reading: the revoke
/// notice, then the reset, then the server's own shutdown, under the receiver's deadline.
async fn epilogue_run() -> (ShutdownReport, bool, ListenerProbe) {
    let (mut server, listener, _wire, _filler) = server_with_stalled_client().await;
    let receiver = TaskGroup::new();
    let reset_ran = Arc::new(AtomicBool::new(false));
    {
        let reset_ran = reset_ran.clone();
        receiver
            .spawn("receiver-routing", async move {
                let notice = serde_json::json!({ "command": "notifySessionEnded" });
                let _ = server.send_to(1, &notice).await;
                reset_ran.store(true, Ordering::SeqCst);
                server.shutdown().await;
            })
            .unwrap();
    }
    let report = receiver
        .shutdown(crate::connect::receiver::SHUTDOWN_DEADLINE)
        .await;
    (report, reset_ran.load(Ordering::SeqCst), listener)
}

#[tokio::test]
async fn the_epilogue_runs_to_its_end_past_a_peer_that_stopped_reading() {
    let (report, reset_ran, _listener) = epilogue_run().await;
    assert!(reset_ran, "the reset after the revoke notice never ran");
    assert_eq!(
        report.graceful_completed,
        vec!["receiver-routing"],
        "the routing task was aborted: {report:?}"
    );
}

#[tokio::test]
async fn the_port_is_free_once_the_receiver_shut_down() {
    let (_report, _reset_ran, listener) = epilogue_run().await;
    // The listener task leaves on its token; give the runtime a turn to drop it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(listener.closed(), "the listener outlived the shutdown");
}

#[tokio::test]
async fn a_heartbeat_on_a_full_queue_reaches_its_verdict() {
    let (write_tx, _write_rx_never_read) = mpsc::channel::<WsMessage>(1);
    write_tx.try_send(WsMessage::Ping(vec![].into())).unwrap();
    let pong_received = Arc::new(AtomicBool::new(true));
    let alive = Arc::new(AtomicBool::new(true));
    let timed_out = Arc::new(AtomicBool::new(false));
    let flag = timed_out.clone();
    let run = crate::connect::ws::heartbeat::run(
        write_tx,
        pong_received,
        alive.clone(),
        move || async move {
            flag.store(true, Ordering::SeqCst);
        },
    );
    let finished = tokio::time::timeout(HEARTBEAT_BOUND, run).await;
    assert!(
        finished.is_ok(),
        "the heartbeat parked in its own Ping send"
    );
    assert!(
        timed_out.load(Ordering::SeqCst),
        "the peer was never declared dead"
    );
    assert!(!alive.load(Ordering::SeqCst));
}

/// The heartbeat as the server runs it, behind the client's real writer parked on the peer.
#[tokio::test]
async fn a_heartbeat_behind_a_stalled_writer_returns() {
    let (client, _wire, _filler) = stalled_client(CancellationToken::new()).await;
    let pong_received = Arc::new(AtomicBool::new(true));
    let alive = Arc::new(AtomicBool::new(true));
    let run = crate::connect::ws::heartbeat::run(
        client.write_tx.clone(),
        pong_received,
        alive,
        || async {},
    );
    let finished = tokio::time::timeout(HEARTBEAT_BOUND, run).await;
    assert!(
        finished.is_ok(),
        "the heartbeat parked behind the stalled writer"
    );
}

/// The receiver's certificate is its own, not one a test client could check: every one passes.
#[derive(Debug)]
struct AcceptAnyCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::CryptoProvider::get_default()
            .map(|p| p.signature_verification_algorithms.supported_schemes())
            .unwrap_or_default()
    }
}

type Peer =
    tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;

/// The server as `WsServer::start` runs it, on the port the kernel gave its listener. Any failure
/// (a build without the server key among them) is the test's answer.
async fn start_on_any_port(events_tx: mpsc::Sender<ClientEvent>) -> (WsServer, u16, ListenerProbe) {
    let (listener, probe) = ProbedListener::bind().await;
    let port = listener.port();
    match WsServer::serve(listener, events_tx) {
        Ok(server) => (server, port, probe),
        Err(e) => panic!("the server did not start: {e:#}"),
    }
}

fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

/// ESTABLISHED server-side sockets on `port`. Read from `/proc/net/tcp`, so Linux only:
/// elsewhere always 0, and the tests below check the tasks alone.
fn established_sockets(port: u16) -> usize {
    #[cfg(target_os = "linux")]
    {
        let want = format!(":{:04X}", port);
        std::fs::read_to_string("/proc/net/tcp")
            .unwrap()
            .lines()
            .skip(1)
            .filter(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                f[1].ends_with(&want) && f[3] == "01"
            })
            .count()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = port;
        0
    }
}

async fn connect_peer(port: u16, rcvbuf: u32) -> Peer {
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.set_recv_buffer_size(rcvbuf).unwrap();
    let tcp = sock
        .connect(format!("127.0.0.1:{port}").parse().unwrap())
        .await
        .unwrap();
    let cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(cfg))
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            tcp,
        )
        .await
        .unwrap();
    tokio_tungstenite::client_async("wss://localhost/", tls)
        .await
        .unwrap()
        .0
}

/// How a client whose writer is parked leaves the server.
#[derive(Clone, Copy, Debug)]
enum Removal {
    /// What `on_timeout` does: cancel the client's token, entry left for the read loop.
    VerdictCancel,
    /// The real heartbeat verdict (missed Pongs), no help from the test.
    VerdictReal,
    /// The peer sends a Close frame, then never reads again.
    PeerClose,
    /// The peer sends a malformed frame (reserved bits set), then never reads again.
    ReadError,
    /// The client is still in the map when the server shuts down.
    Shutdown,
}

/// Asserts that no task or socket of a client whose writer is parked outlives its `removal` or
/// the shutdown.
async fn park_then_remove(removal: Removal) {
    let (events_tx, mut events) = mpsc::channel(64);
    let (mut server, port, _) = start_on_any_port(events_tx).await;
    let base = alive_tasks();

    let mut peer = connect_peer(port, 4096).await;
    let Some(ClientEvent::Connected(id)) = events.recv().await else {
        panic!("no Connected")
    };
    let big = serde_json::json!({ "command": "notifyQueueChanged", "pad": "x".repeat(1 << 20) });
    for _ in 0..4 {
        server.broadcast(&big).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        server.clients.read().await.len(),
        1,
        "evict fired: not the path under test"
    );

    match removal {
        Removal::VerdictCancel => {
            server
                .clients
                .read()
                .await
                .get(&id)
                .unwrap()
                .cancel
                .token()
                .cancel();
        }
        Removal::VerdictReal | Removal::Shutdown => {}
        Removal::PeerClose => {
            let _ = tokio::time::timeout(Duration::from_secs(2), peer.send(WsMessage::Close(None)))
                .await;
        }
        Removal::ReadError => {
            use tokio::io::AsyncWriteExt;
            // FIN + RSV1..3 + Text, masked, empty payload: tungstenite fails the read.
            let raw = peer.get_mut();
            raw.write_all(&[0xF1, 0x80, 1, 2, 3, 4]).await.unwrap();
            raw.flush().await.unwrap();
        }
    }

    if !matches!(removal, Removal::Shutdown) {
        let ev = tokio::time::timeout(Duration::from_secs(45), events.recv()).await;
        assert!(
            matches!(ev, Ok(Some(ClientEvent::Disconnected(_)))),
            "{removal:?}: no Disconnected"
        );
        assert!(
            server.clients.read().await.is_empty(),
            "{removal:?}: entry left in the map"
        );
        // The entry is gone: every task of the client must go with it, without the shutdown.
        let t0 = std::time::Instant::now();
        while alive_tasks() > base && t0.elapsed() < Duration::from_secs(3) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            alive_tasks(),
            base,
            "{removal:?}: a task of the removed client outlived its entry"
        );
        assert_eq!(
            established_sockets(port),
            0,
            "{removal:?}: the removed client's socket is still open"
        );
    }

    server.shutdown().await;
    let t0 = std::time::Instant::now();
    while alive_tasks() > 0 && t0.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        alive_tasks(),
        0,
        "{removal:?}: a task outlived the shutdown"
    );
    assert_eq!(
        established_sockets(port),
        0,
        "{removal:?}: a socket outlived the shutdown"
    );
    drop(peer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_client_ends_its_parked_writer() {
    park_then_remove(Removal::VerdictCancel).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missed_heartbeat_ends_the_parked_writer() {
    park_then_remove(Removal::VerdictReal).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_close_ends_the_parked_writer() {
    park_then_remove(Removal::PeerClose).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_error_ends_the_parked_writer() {
    park_then_remove(Removal::ReadError).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shutdown_ends_the_parked_writer() {
    park_then_remove(Removal::Shutdown).await;
}

/// The server dropped without `shutdown` (its owner aborted or panicked).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_server_releases_its_port() {
    let (events_tx, _events) = mpsc::channel(64);
    let (server, port, listener) = start_on_any_port(events_tx).await;
    let _peer = connect_peer(port, 1 << 16).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(server);
    let t0 = std::time::Instant::now();
    while !listener.closed() && t0.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        listener.closed(),
        "the listener outlived its server ({} tasks alive)",
        alive_tasks()
    );
}

/// What the peer read of a shutdown: the texts, whether the Close frame came, and how the stream
/// ended.
struct Heard {
    texts: Vec<String>,
    closed: bool,
    end: Option<String>,
}

impl Heard {
    /// The server sends no TLS close_notify (tokio-tungstenite never shuts the TLS stream down):
    /// the peer's rustls reports the FIN as an unexpected EOF. Any other end is a reset.
    fn ended_on_a_fin(&self) -> bool {
        self.end
            .as_deref()
            .is_none_or(|e| e.contains("close_notify"))
    }
}

async fn hear_to_the_end(mut read: futures_util::stream::SplitStream<Peer>) -> Heard {
    let mut heard = Heard {
        texts: Vec::new(),
        closed: false,
        end: None,
    };
    while let Some(message) = read.next().await {
        match message {
            Ok(WsMessage::Text(text)) => heard.texts.push(text.to_string()),
            Ok(WsMessage::Close(_)) => heard.closed = true,
            Ok(_) => {}
            Err(e) => {
                heard.end = Some(e.to_string());
                break;
            }
        }
    }
    heard
}

/// Notices large enough to outlast the peer's window, then the session's end, all queued before
/// `shutdown`.
async fn queue_a_session_end(server: &WsServer, socket_id: u32) {
    let pad = "y".repeat(200_000);
    for i in 0..5 {
        let notice = serde_json::json!({ "command": "notifyQueueChanged", "i": i, "pad": pad });
        server.send_to(socket_id, &notice).await.unwrap();
    }
    let end = serde_json::json!({ "command": "notifySessionEnded" });
    server.send_to(socket_id, &end).await.unwrap();
}

fn assert_heard_the_session_end(heard: &Heard) {
    assert_eq!(heard.texts.len(), 6, "every queued notice reached the peer");
    assert!(heard.texts[5].contains("notifySessionEnded"));
    assert!(heard.closed, "the Close frame followed the notices");
    assert!(
        heard.ended_on_a_fin(),
        "the socket was reset: {:?}",
        heard.end
    );
}

/// A server with one TLS peer whose receive window is small, so the flush outlasts it. Returns
/// once the peer's `Connected` is taken from `events`.
async fn server_with_a_peer(
    events_tx: mpsc::Sender<ClientEvent>,
    events: &mut mpsc::Receiver<ClientEvent>,
) -> (WsServer, u32, Peer) {
    let (server, port, _) = start_on_any_port(events_tx).await;
    let peer = connect_peer(port, 1 << 16).await;
    let Some(ClientEvent::Connected(id)) = events.recv().await else {
        panic!("no Connected")
    };
    (server, id, peer)
}

/// Takes every event the server sends until it is dropped, the way the routing loop would.
fn drain(mut events: mpsc::Receiver<ClientEvent>) {
    tokio::spawn(async move { while events.recv().await.is_some() {} });
}

/// The peer reads nothing before the shutdown: it meets the heartbeat's Ping inside the flush,
/// and its Pong reaches a server that has stopped serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pong_that_arrives_during_the_shutdown_leaves_the_queue_whole() {
    let (events_tx, mut events) = mpsc::channel(64);
    let (mut server, id, peer) = server_with_a_peer(events_tx, &mut events).await;
    drain(events);
    queue_a_session_end(&server, id).await;
    let (_write, read) = peer.split();
    let heard = tokio::spawn(hear_to_the_end(read));
    server.shutdown().await;
    let heard = tokio::time::timeout(Duration::from_secs(5), heard)
        .await
        .expect("the peer never saw its socket end")
        .unwrap();
    assert_heard_the_session_end(&heard);
}

/// The Pong is read before the shutdown: the command sent behind it reaches the routing loop, and
/// frames are read in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shutdown_after_the_pong_was_read_leaves_the_queue_whole() {
    let (events_tx, mut events) = mpsc::channel(64);
    let (mut server, id, mut peer) = server_with_a_peer(events_tx, &mut events).await;
    let ping = tokio::time::timeout(Duration::from_secs(5), peer.next())
        .await
        .expect("no Ping from the heartbeat");
    assert!(matches!(ping, Some(Ok(WsMessage::Ping(_)))), "{ping:?}");
    // tungstenite queued the Pong on the Ping: the flush writes it, the marker follows it.
    peer.flush().await.unwrap();
    let marker = serde_json::json!({ "command": "getQueue" });
    peer.send(WsMessage::Text(marker.to_string().into()))
        .await
        .unwrap();
    let read = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("the marker never reached the routing loop");
    assert!(matches!(read, Some(ClientEvent::Message { .. })));
    drain(events);
    queue_a_session_end(&server, id).await;
    let (_write, read) = peer.split();
    let heard = tokio::spawn(hear_to_the_end(read));
    server.shutdown().await;
    let heard = tokio::time::timeout(Duration::from_secs(5), heard)
        .await
        .expect("the peer never saw its socket end")
        .unwrap();
    assert_heard_the_session_end(&heard);
}

/// A phone's command landing while the server flushes: read, never dispatched, since the routing
/// loop has left for its epilogue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_sent_during_the_shutdown_is_read_and_dropped() {
    let (events_tx, mut events) = mpsc::channel(64);
    let (mut server, id, peer) = server_with_a_peer(events_tx, &mut events).await;
    let dispatched = Arc::new(AtomicU32::new(0));
    {
        let dispatched = dispatched.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if let ClientEvent::Message { .. } = event {
                    dispatched.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
    }
    queue_a_session_end(&server, id).await;
    let (mut write, read) = peer.split();
    let heard = tokio::spawn(hear_to_the_end(read));
    let play = serde_json::json!({ "command": "play" });
    let (_, sent) = tokio::join!(
        server.shutdown(),
        write.send(WsMessage::Text(play.to_string().into()))
    );
    assert!(sent.is_ok(), "the command left the peer: {sent:?}");
    let heard = tokio::time::timeout(Duration::from_secs(5), heard)
        .await
        .expect("the peer never saw its socket end")
        .unwrap();
    assert_heard_the_session_end(&heard);
    assert_eq!(
        dispatched.load(Ordering::SeqCst),
        0,
        "a command was dispatched"
    );
}

/// A peer that reads only once its own sends are done, and sends more than the server buffers
/// while the server's queue outgrows the peer's window: each side waits on the other unless the
/// server keeps reading through its flush.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_that_sends_through_the_flush_still_gets_the_whole_queue() {
    let (events_tx, mut events) = mpsc::channel(64);
    let (mut server, id, mut peer) = server_with_a_peer(events_tx, &mut events).await;
    drain(events);
    let pad = "y".repeat(1 << 20);
    for i in 0..16 {
        let notice = serde_json::json!({ "command": "notifyQueueChanged", "i": i, "pad": pad });
        server.send_to(id, &notice).await.unwrap();
    }
    let end = serde_json::json!({ "command": "notifySessionEnded" });
    server.send_to(id, &end).await.unwrap();
    let command = serde_json::json!({ "command": "getQueue", "pad": "z".repeat(1 << 20) });
    let heard = tokio::spawn(async move {
        for _ in 0..16 {
            peer.send(WsMessage::Text(command.to_string().into()))
                .await
                .unwrap();
        }
        hear_to_the_end(peer.split().1).await
    });
    server.shutdown().await;
    let heard = tokio::time::timeout(Duration::from_secs(5), heard)
        .await
        .expect("the peer never saw its socket end")
        .unwrap();
    assert_eq!(
        heard.texts.len(),
        17,
        "every queued notice reached the peer"
    );
    assert!(heard.closed, "the Close frame followed the notices");
    assert!(
        heard.ended_on_a_fin(),
        "the socket was reset: {:?}",
        heard.end
    );
}

/// The routing loop, in its epilogue, reads no command: the read task holds one it cannot hand
/// over when the shutdown starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_parked_on_the_routing_loop_leaves_the_shutdown_its_budget() {
    let (events_tx, mut unread) = mpsc::channel(1);
    let (mut server, id, mut peer) = server_with_a_peer(events_tx, &mut unread).await;
    for i in 0..3 {
        let command = serde_json::json!({ "command": "getQueue", "i": i });
        peer.send(WsMessage::Text(command.to_string().into()))
            .await
            .unwrap();
    }
    // The first command fills the channel, the next one parks the read task in its send.
    let t0 = std::time::Instant::now();
    while unread.is_empty() && t0.elapsed() < Duration::from_secs(5) {
        tokio::task::yield_now().await;
    }
    assert!(!unread.is_empty(), "no command reached the channel");
    queue_a_session_end(&server, id).await;
    let (_write, read) = peer.split();
    let heard = tokio::spawn(hear_to_the_end(read));
    let started = std::time::Instant::now();
    server.shutdown().await;
    assert!(
        started.elapsed() < SERVER_SHUTDOWN_DEADLINE,
        "the shutdown waited out its deadline on a parked read"
    );
    let heard = tokio::time::timeout(Duration::from_secs(5), heard)
        .await
        .expect("the peer never saw its socket end")
        .unwrap();
    assert_heard_the_session_end(&heard);
}

/// A socket's events, from a peer that sends a command and closes while the map is held, the way
/// a broadcast holds it: its connect comes first, alone, and the rest follows once the map is let
/// go, in the order the peer sent it, with no entry left behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_socket_is_heard_connected_before_anything_its_read_loop_sends() {
    let (events_tx, mut events) = mpsc::channel(64);
    let (mut server, port, _) = start_on_any_port(events_tx).await;
    let held = server.clients.read().await;
    let mut peer = connect_peer(port, 1 << 16).await;
    let first = tokio::time::timeout(Duration::from_secs(5), events.recv()).await;
    let Ok(Some(ClientEvent::Connected(id))) = first else {
        panic!("the connect was not heard first")
    };
    let command = serde_json::json!({ "command": "getQueue" });
    peer.send(WsMessage::Text(command.to_string().into()))
        .await
        .unwrap();
    peer.close(None).await.unwrap();
    let early = tokio::time::timeout(Duration::from_millis(300), events.recv()).await;
    assert!(
        early.is_err(),
        "the read loop ran before the socket's entry was in"
    );
    drop(held);
    let message = tokio::time::timeout(Duration::from_secs(5), events.recv()).await;
    assert!(
        matches!(message, Ok(Some(ClientEvent::Message { socket_id, .. })) if socket_id == id),
        "the command did not follow the connect"
    );
    let end = tokio::time::timeout(Duration::from_secs(5), events.recv()).await;
    assert!(
        matches!(end, Ok(Some(ClientEvent::Disconnected(socket_id))) if socket_id == id),
        "the disconnect did not follow the command"
    );
    assert!(
        server.clients.read().await.is_empty(),
        "the entry outlived its read loop"
    );
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_evicted_client_is_heard_disconnected_once() {
    let (events_tx, mut events) = mpsc::channel(64);
    let (mut server, port, _) = start_on_any_port(events_tx).await;
    let _peer = connect_peer(port, 4096).await;
    let Some(ClientEvent::Connected(id)) = events.recv().await else {
        panic!("no Connected")
    };
    // The peer never reads: the writer parks on its window and the queue fills behind it.
    let big = serde_json::json!({ "command": "notifyQueueChanged", "pad": "x".repeat(1 << 16) });
    let t0 = std::time::Instant::now();
    while !server.clients.read().await.is_empty() && t0.elapsed() < Duration::from_secs(10) {
        server.broadcast(&big).await;
    }
    assert!(server.clients.read().await.is_empty(), "never evicted");
    let end = tokio::time::timeout(Duration::from_secs(5), events.recv()).await;
    assert!(
        matches!(end, Ok(Some(ClientEvent::Disconnected(socket_id))) if socket_id == id),
        "the eviction was never heard"
    );
    let again = tokio::time::timeout(Duration::from_millis(500), events.recv()).await;
    assert!(again.is_err(), "the socket's end was heard twice");
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_that_never_answers_the_close_holds_the_shutdown_to_its_deadline() {
    use tokio::io::AsyncReadExt;
    let (events_tx, mut events) = mpsc::channel(64);
    let (mut server, id, peer) = server_with_a_peer(events_tx, &mut events).await;
    drain(events);
    queue_a_session_end(&server, id).await;
    // Below the WebSocket layer: the peer reads, and never writes a Pong or a Close.
    let mut tls = peer.into_inner();
    let heard = tokio::spawn(async move {
        let mut bytes = Vec::new();
        let mut buf = vec![0u8; 1 << 16];
        while let Ok(n) = tls.read(&mut buf).await {
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
        }
        bytes
    });
    let started = std::time::Instant::now();
    server.shutdown().await;
    let took = started.elapsed();
    let bytes = tokio::time::timeout(Duration::from_secs(5), heard)
        .await
        .expect("the peer never saw its socket end")
        .unwrap();
    assert!(
        took < SERVER_SHUTDOWN_DEADLINE + Duration::from_millis(500),
        "the shutdown outlived its deadline: {took:?}"
    );
    assert!(
        String::from_utf8_lossy(&bytes).contains("notifySessionEnded"),
        "the session's end never reached the peer"
    );
    assert!(bytes.ends_with(&[0x88, 0x00]), "the Close frame came last");
}

#[tokio::test]
async fn reading_to_the_close_stops_at_the_peers_close() {
    let mut frames = futures_util::stream::iter(vec![
        Ok::<_, ()>(WsMessage::Pong(vec![].into())),
        Ok(WsMessage::Text("play".into())),
        Ok(WsMessage::Close(None)),
        Ok(WsMessage::Text("after".into())),
    ]);
    read_to_close(&mut frames).await;
    assert_eq!(
        frames.next().await,
        Some(Ok(WsMessage::Text("after".into()))),
        "the frame after the Close is left unread"
    );
}
