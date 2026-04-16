//! Shared websocket ping/pong heartbeat driver.
//!
//! Both `ws::client` and `ws::server` need the same loop: send a Ping
//! every `PING_INTERVAL_MS`, require a Pong within the remaining
//! `PING_TIMEOUT_MS - PING_INTERVAL_MS` window, and treat a missed Pong as
//! a dead connection. The pre-split code duplicated this loop in both
//! modules; this module centralises it and takes the per-side disconnection
//! action as a closure (`on_timeout`).
//!
//! The `alive` flag is the shared "connection is up" bit. The driver
//! flips it to `false` before invoking `on_timeout`, so observers that
//! poll `alive` see the disconnection exactly once.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::connect::consts;

/// Run the heartbeat loop until `alive` is cleared or the write channel
/// closes. On a missed Pong, `alive` is set to `false`, `on_timeout` is
/// awaited, and the loop exits.
pub(crate) async fn run<F, Fut>(
    write_tx: mpsc::Sender<WsMessage>,
    pong_received: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    on_timeout: F,
) where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    let mut interval = tokio::time::interval(Duration::from_millis(consts::PING_INTERVAL_MS));

    loop {
        interval.tick().await;
        if !alive.load(Ordering::Relaxed) {
            break;
        }

        pong_received.store(false, Ordering::Relaxed);
        if write_tx.send(WsMessage::Ping(vec![].into())).await.is_err() {
            break;
        }

        let pong_wait = Duration::from_millis(consts::PING_TIMEOUT_MS - consts::PING_INTERVAL_MS);
        tokio::time::sleep(pong_wait).await;

        if !pong_received.load(Ordering::Relaxed) && alive.load(Ordering::Relaxed) {
            alive.store(false, Ordering::Relaxed);
            on_timeout().await;
            break;
        }
    }
}
