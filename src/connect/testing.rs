//! Test-only helpers for the connect module.
//!
//! `MockWsServer` is a plain-WS (not WSS) server that listens on a
//! randomly-assigned localhost port and stands in for a mobile device:
//! the code under test dials in, and the accepted connection stays open
//! for as long as the test holds it. TLS is intentionally out
//! of scope here: the crypto layer is orthogonal to the protocol
//! state-machine logic under test, and a plain server avoids the
//! need for test certificates.
//!
//! Typical use:
//!
//! ```ignore
//! let mut server = MockWsServer::new().await;
//! let url = server.url();             // connect the SUT to this
//! let _conn = server.accept().await;  // wait for the SUT to dial in, and keep it open
//! ```

#![cfg(test)]

use std::net::SocketAddr;

use anyhow::{Context, Result};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
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
                if tx.send(MockWsConnection { _stream: ws }).await.is_err() {
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

/// An accepted server-side websocket connection, open for as long as it is held.
pub struct MockWsConnection {
    _stream: WebSocketStream<TcpStream>,
}
