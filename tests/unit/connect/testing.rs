//! Tests for `src/connect/testing.rs`, attached to it by `#[path]`.
//!
//! The mock server itself stays in the source file: it is test support the rest of
//! the `connect` tests import, not a test.

use super::*;
use tokio_tungstenite::connect_async;

/// Smoke test: a client dialing the mock can complete a roundtrip.
#[tokio::test]
async fn client_roundtrip_via_mock() {
    let mut server = MockWsServer::new().await.unwrap();
    let url = server.url();

    let client_task = tokio::spawn(async move {
        let (mut ws, _resp) = connect_async(url).await.unwrap();
        ws.send(Message::Text("hello".into())).await.unwrap();
        let reply = ws.next().await.unwrap().unwrap();
        match reply {
            Message::Text(s) => assert_eq!(s, "world"),
            other => panic!("unexpected: {other:?}"),
        }
    });

    let mut conn = server.accept().await.expect("accept");
    let got = conn.expect_text().await.unwrap();
    assert_eq!(got, "hello");
    conn.send_text("world").await.unwrap();

    client_task.await.unwrap();
}

/// A second connection can be accepted after the first.
#[tokio::test]
async fn server_accepts_multiple_connections() {
    let mut server = MockWsServer::new().await.unwrap();
    let url = server.url();

    let u1 = url.clone();
    let c1 = tokio::spawn(async move {
        let (mut ws, _) = connect_async(u1).await.unwrap();
        ws.send(Message::Text("a".into())).await.unwrap();
    });
    let u2 = url.clone();
    let c2 = tokio::spawn(async move {
        let (mut ws, _) = connect_async(u2).await.unwrap();
        ws.send(Message::Text("b".into())).await.unwrap();
    });

    let mut conn1 = server.accept().await.unwrap();
    let mut conn2 = server.accept().await.unwrap();
    // Order is not guaranteed; just check both arrive.
    let msg1 = conn1.expect_text().await.unwrap();
    let msg2 = conn2.expect_text().await.unwrap();
    let mut received = [msg1, msg2];
    received.sort();
    assert_eq!(received, ["a".to_string(), "b".to_string()]);

    c1.await.unwrap();
    c2.await.unwrap();
}
