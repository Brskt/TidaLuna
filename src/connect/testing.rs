//! Test-only helpers for the connect module.
//!
//! `MockWsServer` is a plain-WS (not WSS) server that listens on a
//! randomly-assigned localhost port so unit tests can exercise the
//! parts of the TIDAL Connect protocol that depend on websocket
//! framing without a real mobile device. TLS is intentionally out
//! of scope here: the crypto layer is orthogonal to the protocol
//! state-machine logic under test, and a plain server avoids the
//! need for test certificates.
//!
//! Typical use:
//!
//! ```ignore
//! let mut server = MockWsServer::new().await;
//! let url = server.url();                // connect the SUT to this
//! let mut conn = server.accept().await;  // wait for the SUT to dial in
//! conn.send_text(r#"{"command":"ping"}"#).await.unwrap();
//! let msg = conn.expect_text().await;
//! assert!(msg.contains("pong"));
//! ```

#![cfg(test)]

use std::net::SocketAddr;

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{WebSocketStream, accept_async};

/// A mock websocket server bound to a random loopback port.
///
/// The accept loop runs on a background task; incoming connections
/// are handed to the caller via `accept()`.
pub struct MockWsServer {
    addr: SocketAddr,
    incoming_rx: mpsc::Receiver<MockWsConnection>,
    _accept_task: JoinHandle<()>,
}

impl MockWsServer {
    /// Bind to `127.0.0.1:0`, start an accept loop, and return a handle.
    pub async fn new() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind mock ws listener")?;
        let addr = listener.local_addr().context("local_addr")?;

        let (tx, incoming_rx) = mpsc::channel::<MockWsConnection>(8);

        let accept_task = tokio::spawn(async move {
            loop {
                let (stream, _peer) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let ws = match accept_async(stream).await {
                    Ok(ws) => ws,
                    Err(_) => continue,
                };
                if tx.send(MockWsConnection { stream: ws }).await.is_err() {
                    // Receiver dropped, server is shutting down.
                    break;
                }
            }
        });

        Ok(Self {
            addr,
            incoming_rx,
            _accept_task: accept_task,
        })
    }

    /// `ws://host:port` URL the SUT should dial.
    pub fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// Wait for the next incoming connection. Returns `None` if the
    /// server was dropped or the accept loop died.
    pub async fn accept(&mut self) -> Option<MockWsConnection> {
        self.incoming_rx.recv().await
    }
}

/// An accepted server-side websocket connection.
pub struct MockWsConnection {
    stream: WebSocketStream<TcpStream>,
}

impl MockWsConnection {
    pub async fn send_text(&mut self, text: impl Into<String>) -> Result<()> {
        self.stream
            .send(Message::Text(text.into().into()))
            .await
            .context("send text frame")
    }

    /// Next frame from the peer, if any. Returns `None` on clean close.
    pub async fn recv(&mut self) -> Result<Option<Message>> {
        match self.stream.next().await {
            Some(Ok(msg)) => Ok(Some(msg)),
            Some(Err(e)) => Err(e).context("recv frame"),
            None => Ok(None),
        }
    }

    /// Next frame, expected to be a text frame. Errors otherwise.
    pub async fn expect_text(&mut self) -> Result<String> {
        match self.recv().await? {
            Some(Message::Text(s)) => Ok(s.to_string()),
            Some(other) => Err(anyhow!("expected text frame, got {other:?}")),
            None => Err(anyhow!("connection closed before text frame")),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
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
}
