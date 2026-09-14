//! Tests for the failure text the load paths publish, attached to them by `#[path]`.
//!
//! A media url's query is a time-bounded CDN credential, and `reqwest::Error`'s own `Display`
//! appends the whole url. None of these strings stays in Rust: each rides `MediaError` into the
//! renderer's JS realm, which is unconditional, and out over the Connect socket to every
//! controller on the network.
//!
//! `tests/unit/util.rs` already pins `network_error_text` itself. That test cannot fail for a
//! call site that never calls it, which is how five arms in `src/player/mod.rs` went raw while
//! the helper sat one module away. These drive the real functions instead.

use super::{
    LoadContext, LoadOrigin, Player, PlayerCommand, PlayerEvent, ResumePolicy, start_stream_load,
};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// Binds a listener that answers one request with a complete, empty 200, then closes. Nothing
/// between the init fetch and the segment loop examines the bytes it returned, so an empty body
/// is a perfectly successful init and the segment arms become reachable.
async fn complete_response_port() -> u16 {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 1024];
        let _ = sock.read(&mut request).await;
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        let _ = sock.flush().await;
    });
    port
}

/// Binds a listener that promises a megabyte, delivers ten bytes, then drops the socket. The
/// unfinished body is what fails `r.bytes()`, and that is a different arm from a refused
/// connection: reqwest re-attaches the url when it wraps a body error, where the streaming API
/// never carries one at all. Testing only the refused-connection shape would miss it.
async fn truncated_response_port() -> u16 {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 1024];
        let _ = sock.read(&mut request).await;
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\nten bytes.")
            .await;
        let _ = sock.flush().await;
    });
    port
}

/// A `Player` whose events land in a vector the caller can poll.
///
/// Construction opens no audio device and needs no CEF. It does spawn the command thread, and
/// that thread has no shutdown path: `PlayerThread::run` is a loop with no exit, `Player` has no
/// `Drop`, and the `JoinHandle` is discarded at birth. Each of these lives until the test binary
/// does. That cost buys the only route to `load_dash`, which unlike `start_stream_load` is a
/// method whose fetch lives inline in a spawned closure.
fn spying_player() -> (Player, Arc<Mutex<Vec<PlayerEvent>>>) {
    let events: Arc<Mutex<Vec<PlayerEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let player = Player::new(
        move |ev| sink.lock().unwrap_or_else(|e| e.into_inner()).push(ev),
        tokio::runtime::Handle::current(),
    )
    .expect("construction opens no device and cannot fail off Windows");
    (player, events)
}

/// Waits for the load to settle and hands back its failure text. The fetch runs on the runtime
/// while the callback fires from the command thread, so the event lands on neither of them;
/// polling a spy against a deadline is how the rest of this suite waits on the same shape.
async fn settled_error(events: &Arc<Mutex<Vec<PlayerEvent>>>) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let seen = events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find_map(|ev| match ev {
                PlayerEvent::MediaError { error, .. } => Some(error.clone()),
                _ => None,
            });
        if let Some(error) = seen {
            return error;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the load never settled into a MediaError"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// `SECRET` stands in for the signature every real media url carries, and the host for the CDN
/// edge. A failure text may name neither, and still has to say something a listener can act on.
fn assert_no_credential(error: &str) {
    assert!(
        !error.contains("SECRET"),
        "the credential survived into the failure text: {error}"
    );
    assert!(
        !error.contains("127.0.0.1"),
        "the url survived into the failure text: {error}"
    );
    assert!(
        !error.is_empty(),
        "the failure still has to say something a listener can act on"
    );
}

/// Every load in this file mints a generation, and both `is_stale()` and the `LoadFailed`
/// handler drop anything whose generation is no longer current. A mint from a parallel test
/// would swallow the event under test and leave the poll to time out against a defect that
/// was never there.
async fn serialised() -> tokio::sync::MutexGuard<'static, ()> {
    crate::audio::preload::tests::PRELOAD_TESTS.lock().await
}

#[tokio::test]
async fn a_refused_connection_publishes_no_credential() {
    let _serialised = serialised().await;

    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<PlayerCommand>();
    let ctx = LoadContext {
        load_gen: crate::player::current_gen(),
        event_seq: 0,
        load_start: std::time::Instant::now(),
        resume_policy: ResumePolicy::Disabled,
        auto_play: true,
        cmd_tx,
        format: "flac".to_string(),
        product_id: None,
        cancel_token: CancellationToken::new(),
    };

    // Port 1 refuses the connection, failing the request before a response exists.
    let url = "http://127.0.0.1:1/track.flac?token=SECRET&exp=1";
    start_stream_load(&ctx, url, "", "track-a").await;

    let Ok(PlayerCommand::LoadFailed { error, .. }) = cmd_rx.try_recv() else {
        panic!("a refused connection has to settle the load, and nothing else was published");
    };
    assert_no_credential(&error);
}

#[tokio::test]
async fn a_refused_dash_init_publishes_no_credential() {
    let _serialised = serialised().await;
    let (player, events) = spying_player();

    // The segment list is never fetched on this arm, but it cannot be empty: `load_dash` bails
    // on an empty one before it mints a load at all.
    player
        .load_dash(
            "http://127.0.0.1:1/init.mp4?token=SECRET&exp=1".to_string(),
            vec!["http://127.0.0.1:1/seg-0.m4s".to_string()],
            "flac".to_string(),
            None,
            LoadOrigin::Local,
        )
        .expect("a non-empty segment list is accepted");

    assert_no_credential(&settled_error(&events).await);
}

#[tokio::test]
async fn a_truncated_dash_init_publishes_no_credential() {
    let _serialised = serialised().await;
    let (player, events) = spying_player();
    let init = truncated_response_port().await;

    player
        .load_dash(
            format!("http://127.0.0.1:{init}/init.mp4?token=SECRET&exp=1"),
            vec!["http://127.0.0.1:1/seg-0.m4s".to_string()],
            "flac".to_string(),
            None,
            LoadOrigin::Local,
        )
        .expect("a non-empty segment list is accepted");

    assert_no_credential(&settled_error(&events).await);
}

#[tokio::test]
async fn a_refused_dash_segment_publishes_no_credential() {
    let _serialised = serialised().await;
    let (player, events) = spying_player();
    let init = complete_response_port().await;

    // One segment is enough, and it is also the safe count: `buffered` holds up to six futures
    // at once, and the loop returns on the first error without draining the rest.
    player
        .load_dash(
            format!("http://127.0.0.1:{init}/init.mp4"),
            vec!["http://127.0.0.1:1/seg-0.m4s?token=SECRET&exp=1".to_string()],
            "flac".to_string(),
            None,
            LoadOrigin::Local,
        )
        .expect("a non-empty segment list is accepted");

    assert_no_credential(&settled_error(&events).await);
}

#[tokio::test]
async fn a_truncated_dash_segment_publishes_no_credential() {
    let _serialised = serialised().await;
    let (player, events) = spying_player();
    let init = complete_response_port().await;
    let segment = truncated_response_port().await;

    player
        .load_dash(
            format!("http://127.0.0.1:{init}/init.mp4"),
            vec![format!(
                "http://127.0.0.1:{segment}/seg-0.m4s?token=SECRET&exp=1"
            )],
            "flac".to_string(),
            None,
            LoadOrigin::Local,
        )
        .expect("a non-empty segment list is accepted");

    assert_no_credential(&settled_error(&events).await);
}
