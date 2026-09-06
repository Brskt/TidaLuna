//! Tests for the pre-buffer wait in `start_stream_load`, attached to it by `#[path]`.

use super::{LoadContext, PlayerCommand, ResumePolicy, start_stream_load};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// A download that dies before the pre-buffer target must not cost the load the deadline.
///
/// This wait reads the byte count and the clock, and neither of them says the writer is
/// gone: a download that failed at byte zero used to hold the load here for the whole
/// `PRE_BUFFER_TIMEOUT_MS`, then publish what was never going to arrive anyway.
///
/// What it must STILL do is publish. The buffer carries the failure, `RamBuffer::read` hands
/// it back on the first read before any range check, and the decode thread turns that into a
/// listener-visible error. A wait that refused to publish would swallow the only report of
/// it. This test pins the publish as hard as it pins the delay.
#[tokio::test]
async fn a_download_that_dies_before_the_pre_buffer_target_still_publishes_at_once() {
    // Every minter of a load generation in the suite holds this lock, and `is_stale()` reads
    // that generation: a mint landing between the snapshot below and the call would end this
    // load before it started, and the test would be measuring the wrong exit.
    let _serialised = crate::audio::preload::tests::PRELOAD_TESTS.lock().await;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 1024];
        let _ = sock.read(&mut request).await;
        // A length past the pre-buffer target, for the load to take the streaming branch
        // rather than the whole-copy one, and a body that never comes.
        sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n")
            .await
            .unwrap();
        sock.flush().await.unwrap();
        std::future::pending::<()>().await;
    });

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

    let url = format!("http://127.0.0.1:{port}/track.flac");
    let loaded_at = std::time::Instant::now();
    // A key no decryptor can be built from ends the download at byte zero, which is the shape
    // every early death shares: nothing written, and no writer left to write.
    start_stream_load(&ctx, &url, "!!! not a tidal key !!!", "track-a").await;

    assert!(
        loaded_at.elapsed() < std::time::Duration::from_secs(1),
        "the load sat out its pre-buffer deadline on a download that had already failed"
    );
    assert!(
        matches!(cmd_rx.try_recv(), Ok(PlayerCommand::Load { .. })),
        "the buffer carries the failure, and not publishing it loses the only report there is"
    );
}
