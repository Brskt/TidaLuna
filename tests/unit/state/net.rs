//! Tests for `src/state/net.rs`, attached to it by `#[path]`.

use super::*;
use crate::state::HANG_GUARD;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Only a hang reaches it in real time. The bound under test elapses on a paused clock instead.
const SILENCE: Duration = HANG_GUARD;

/// What test 2's fixture sends before it goes silent.
const SENT: usize = 1024;

/// Answers one request with a head, then with `body` until the socket fails, then stays open
/// without a byte more. Returns the url to fetch.
async fn media_server(body: Option<usize>) -> String {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 1024];
        let _ = sock.read(&mut request).await;
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1073741824\r\n\r\n")
            .await;
        match body {
            // As a CDN does: only TCP flow control holds it back while the client pauses.
            None => while sock.write_all(&[0u8; 16 * 1024]).await.is_ok() {},
            Some(len) => {
                let _ = sock.write_all(&vec![0u8; len]).await;
            }
        }
        let _ = sock.flush().await;
        std::future::pending::<()>().await;
    });
    format!("http://127.0.0.1:{port}/track.flac")
}

// The governor holds a download between two reads for as long as playback is paused. reqwest
// 0.13.5's `read_timeout` counted that wait, and every resume after 15 s reconnected for nothing.
#[tokio::test]
async fn a_pause_between_two_reads_costs_nothing() {
    let client = MediaClient::with_silence(SILENCE);
    let mut body = client
        .get(media_server(None).await)
        .send()
        .await
        .expect("the fixture answers")
        .body();
    body.next()
        .await
        .expect("a first frame")
        .expect("the first read succeeds");

    // The pause outlasts the bound in virtual time, while the read after it runs on the real clock.
    tokio::time::pause();
    tokio::time::advance(SILENCE * 2).await;
    tokio::time::resume();

    let after = body.next().await;
    assert!(
        matches!(after, Some(Ok(_))),
        "a read after the pause failed: {after:?}"
    );
}

// Without the bound, a server that keeps the connection and stops sending holds the download
// for good.
#[tokio::test]
async fn a_body_that_goes_silent_ends_at_the_bound() {
    let client = MediaClient::with_silence(SILENCE);
    let mut body = client
        .get(media_server(Some(SENT)).await)
        .send()
        .await
        .expect("the fixture answers")
        .body();
    let mut received = 0;
    while received < SENT {
        received += body
            .next()
            .await
            .expect("the body goes on")
            .expect("a read before the silence succeeds")
            .len();
    }

    // Every byte is in: what remains is the silence, and it elapses in virtual time.
    tokio::time::pause();
    let outcome = tokio::time::timeout(SILENCE * 4, async {
        loop {
            match body.next().await {
                Some(Ok(_)) => continue,
                other => break other,
            }
        }
    })
    .await
    .expect("a silent body must end");
    assert!(
        matches!(outcome, Some(Err(AudioFetchError::Stalled))),
        "a silent body ended otherwise: {outcome:?}"
    );
}
