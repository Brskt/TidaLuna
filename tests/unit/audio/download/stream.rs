//! Tests for `src/audio/download/stream.rs`, attached to it by `#[path]`.

use super::*;
use crate::audio::bandwidth::GovernorHandle;
use crate::player::buffer::DownloadOwner;
use crate::state::{HANG_GUARD, warm_process_globals};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// A seek past the frontier restarts the download, and a request that cannot leave (refused port,
/// dead route) makes `reqwest` stamp its own error with the url it tried. The bypass packet loops
/// carry that into `MediaError`, which crosses into the renderer's JS realm and out over the
/// Connect socket, and the query is a time-bounded credential.
#[tokio::test]
async fn a_refused_range_restart_publishes_no_credential() {
    use std::io::{Read as _, Seek as _};
    warm_process_globals();

    // Answers a head, then stops talking: the download parks on `stream.next()`, which is the
    // state a restart interrupts. The length it announces never arrives.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 1024];
        let _ = sock.read(&mut request).await;
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n")
            .await;
        let _ = sock.write_all(&[0u8; 1024]).await;
        let _ = sock.flush().await;
        std::future::pending::<()>().await;
    });

    let resp = crate::state::HTTP_CLIENT_PRELOAD
        .get(format!("http://127.0.0.1:{port}/track.flac"))
        .send()
        .await
        .expect("the fixture accepts the first request");

    let (buffer, writer) = crate::player::buffer::RamBuffer::new(
        1_048_576,
        DownloadOwner::Preload,
        CancellationToken::new(),
        GovernorHandle::ungoverned(),
    );

    // `download_stream` never issues the first request - it is handed one already open. The
    // restart resolves its target from this argument instead, so a refused port here fails the
    // restart alone while the transfer already in flight stays healthy.
    let download = tokio::spawn(download_stream(
        resp,
        "http://127.0.0.1:1/track.flac?token=SECRET&exp=1".to_string(),
        String::new(),
        writer,
    ));

    // The head must land before the seek: a cursor inside the lookahead makes the reader wait
    // for bytes rather than move the download, and no restart would ever be armed.
    tokio::time::timeout(HANG_GUARD, async {
        while !matches!(
            buffer.head_status(1024),
            crate::player::buffer::HeadStatus::Landed
        ) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the fixture never delivered its first chunk");

    // Past the frontier plus the 32 KiB lookahead. The read parks, so it cannot share the
    // runtime that is driving the download.
    let mut reader = buffer.clone();
    std::thread::spawn(move || {
        reader
            .seek(std::io::SeekFrom::Start(1024 + 64 * 1024))
            .expect("seeking a RamBuffer moves its cursor and nothing else");
        let mut sink = [0u8; 64];
        let _ = reader.read(&mut sink);
    });

    download.await.expect("the download task must not panic");

    let mut settled = buffer.clone();
    let mut sink = [0u8; 64];
    let text = settled
        .read(&mut sink)
        .expect_err("a refused restart ends the download, and every later read carries that")
        .to_string();

    assert!(
        !text.contains("SECRET"),
        "the credential survived into the failure text: {text}"
    );
    assert!(
        !text.contains("127.0.0.1"),
        "the url survived into the failure text: {text}"
    );
}
