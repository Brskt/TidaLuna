//! Shared websocket ping/pong heartbeat driver.
//!
//! Both `ws::client` and `ws::server` need the same loop: send a Ping
//! every `PING_INTERVAL_MS` and treat a peer that has not answered for
//! roughly `PING_TIMEOUT_MS` as dead. The ping cadence and the dead-peer
//! deadline are kept independent: a Pong is awaited across each interval
//! and the peer is dropped only after `PING_TIMEOUT_MS / PING_INTERVAL_MS`
//! consecutive intervals without one, so the wait never stretches the ping
//! period. The pre-split code duplicated this loop in both modules; this
//! module centralises it and takes the per-side disconnection action as a
//! closure (`on_timeout`).
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
    // Steady ping cadence: one Ping every `PING_INTERVAL_MS`. `Delay` keeps the
    // period steady instead of firing a catch-up burst when a tick is late.
    let mut ping_tick = tokio::time::interval(Duration::from_millis(consts::PING_INTERVAL_MS));
    ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick completes immediately; consume it so the first Ping is sent
    // right away and later ones are spaced by exactly one interval.
    ping_tick.tick().await;

    // A peer that misses this many consecutive Pong windows is declared dead,
    // approximating `PING_TIMEOUT_MS` without coupling the wait to the cadence.
    let max_missed = (consts::PING_TIMEOUT_MS / consts::PING_INTERVAL_MS).max(1);
    let mut missed: u64 = 0;

    loop {
        if !alive.load(Ordering::Relaxed) {
            break;
        }
        if write_tx.send(WsMessage::Ping(vec![].into())).await.is_err() {
            break;
        }
        // Clear only after the Ping is enqueued, so a failed send exits above
        // without arming the missed-Pong check.
        pong_received.store(false, Ordering::Relaxed);

        ping_tick.tick().await;
        if !alive.load(Ordering::Relaxed) {
            break;
        }

        if pong_received.load(Ordering::Relaxed) {
            missed = 0;
        } else {
            missed += 1;
            if missed >= max_missed {
                alive.store(false, Ordering::Relaxed);
                on_timeout().await;
                break;
            }
        }
    }
}
