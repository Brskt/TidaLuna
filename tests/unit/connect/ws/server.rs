//! Tests for `src/connect/ws/server.rs`, attached to it by `#[path]`.

use super::*;

// The accept must observe a cancelled token instead of blocking forever in
// accept() when no connection arrives. Driven at the helper so it needs no
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
