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
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{WebSocketStream, accept_async};

use crate::connect::controller::TidalConnectController;
use crate::connect::types::MdnsDevice;
use crate::connect::ws::client::WsClient;

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

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Wait for the next incoming connection. Returns `None` if the
    /// server was dropped or the accept loop died.
    pub async fn accept(&mut self) -> Option<MockWsConnection> {
        self.incoming_rx.recv().await
    }
}

/// A controller with a session on `device` that TIDAL was told started, the generation its
/// socket was installed at, and the connection that keeps that socket open.
pub async fn live_controller(
    server: &mut MockWsServer,
    device: &MdnsDevice,
) -> (Arc<Mutex<TidalConnectController>>, u64, MockWsConnection) {
    let port: u16 = server.url().rsplit(':').next().unwrap().parse().unwrap();
    let (event_tx, _events) = mpsc::channel(8);
    let dialing =
        tokio::spawn(async move { WsClient::connect("127.0.0.1", port, true, event_tx).await });
    let conn = server.accept().await.expect("accept");
    let ws = Arc::new(dialing.await.unwrap().expect("dial"));

    let mut ctrl = TidalConnectController::new(None);
    let adopted = ctrl.adopt_started(ws, device, "s1".to_string(), false);
    (Arc::new(Mutex::new(ctrl)), adopted.conn_gen, conn)
}

/// An accepted server-side websocket connection, open for as long as it is held.
pub struct MockWsConnection {
    stream: WebSocketStream<TcpStream>,
}

impl MockWsConnection {
    /// The next frame the code under test sent, `None` once the connection is closed or a frame
    /// is not JSON.
    pub async fn recv(&mut self) -> Option<serde_json::Value> {
        loop {
            match self.stream.next().await? {
                Ok(Message::Text(text)) => return serde_json::from_str(&text).ok(),
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    /// Sends the code under test a frame, as the device would.
    pub async fn send(&mut self, frame: serde_json::Value) {
        self.stream
            .send(Message::Text(frame.to_string().into()))
            .await
            .expect("send to the code under test");
    }
}
