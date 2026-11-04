//! Tests for `src/connect/ws/client.rs`, attached to it by `#[path]`.

use super::*;
use crate::connect::testing::MockWsServer;
use std::time::Duration;

/// Longer than any bound a close is held to: past it, no task of the socket may be left.
const WITHIN: Duration = Duration::from_secs(10);

fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

async fn all_tasks_gone(base: usize) -> bool {
    let deadline = tokio::time::Instant::now() + WITHIN;
    while alive_tasks() > base && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    alive_tasks() <= base
}

/// A client dialled to `port`, its events dropped.
async fn dial(port: u16) -> WsClient {
    let (event_tx, _events) = mpsc::channel(8);
    WsClient::connect("127.0.0.1", port, true, event_tx)
        .await
        .unwrap()
}

/// What was queued when the last holder let go is written, then the Close follows: a client closed
/// on the spot would drop the queue, and the end of a session sent a moment before with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_last_holder_writes_what_is_queued_then_closes() {
    let mut server = MockWsServer::new().await.unwrap();
    let base = alive_tasks();
    let dialing = tokio::spawn(dial(server.port()));
    let mut conn = server.accept().await.expect("accept");
    let ws = dialing.await.unwrap();
    // Past the heartbeat's first Ping: it is parked in its tick.
    tokio::time::sleep(Duration::from_millis(300)).await;
    for i in 0..3 {
        ws.send_raw(serde_json::json!({ "command": format!("m{i}") }))
            .await
            .unwrap();
    }

    drop(ws);

    assert_eq!(
        conn.wire_until_end(Duration::from_secs(5)).await,
        vec!["m0", "m1", "m2", "<close>", "<eof>"]
    );
    assert!(
        all_tasks_gone(base).await,
        "the heartbeat outlived the client"
    );
}

/// The reply to the peer's Close owes nothing to the writer, whose close a frame it had yet to
/// flush makes fail before the reply goes out. Here no writer runs at all: the reader writes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reader_answers_the_peers_close_without_the_writer() {
    let mut server = MockWsServer::new().await.unwrap();
    let url = server.url();
    let dialing =
        tokio::spawn(async move { tokio_tungstenite::connect_async(url).await.unwrap().0 });
    let mut conn = server.accept().await.expect("accept");
    let (_write, read) = dialing.await.unwrap().split();
    let (event_tx, _events) = mpsc::channel(8);
    tokio::spawn(read_task(
        read,
        Arc::new(PendingRequests::new()),
        event_tx,
        Arc::default(),
        Arc::default(),
        CancellationToken::new(),
        CancellationToken::new(),
    ));

    assert_eq!(
        conn.close_then_wire_until_end(Duration::from_secs(5)).await,
        vec!["<close>", "<eof>"]
    );
}

/// The close is bounded: a device that stopped reading, its window full, never answers the Close
/// and holds the writer parked in a send. The deadline has to cancel it, as the read side that was
/// to end the close cannot free it, and leave no task of the socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_that_stopped_reading_does_not_hold_the_writer() {
    let base = alive_tasks();
    let (ws, _peer) = parked_writer().await;

    drop(ws);

    assert!(all_tasks_gone(base).await, "a task outlived the deadline");
}

/// A client whose writer is parked in a send to a peer whose window is full, and that peer.
async fn parked_writer() -> (
    WsClient,
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(1).unwrap();
    let port = listener.local_addr().unwrap().port();
    let dialing = tokio::spawn(dial(port));
    let (tcp, _) = listener.accept().await.unwrap();
    let peer = tokio_tungstenite::accept_async(tcp).await.unwrap();
    let ws = dialing.await.unwrap();
    // Past what the socket buffers hold: the writer parks, the rest waits in the queue.
    let pad = "y".repeat(256 * 1024);
    for _ in 0..40 {
        ws.queue_command(serde_json::json!({ "command": "notifyQueueChanged", "pad": pad }))
            .unwrap();
    }
    (ws, peer)
}

/// The read side ends on the peer's Close while the writer is parked: nothing can be written after
/// it, and the end of reading frees the writer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_that_stopped_reading_and_closed_does_not_hold_the_writer() {
    use futures_util::SinkExt;
    let base = alive_tasks();
    let (ws, mut peer) = parked_writer().await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    peer.send(tokio_tungstenite::tungstenite::Message::Close(None))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(ws);

    assert!(
        all_tasks_gone(base).await,
        "the writer outlived the end of reading ({} tasks)",
        alive_tasks()
    );
}
