//! Player-to-receiver bridge state and event forwarding.
//!
//! When the TIDAL Connect receiver is active on this host, player events
//! (state changes, time updates, errors) are translated to `BridgeEvent`s
//! and forwarded to the receiver's playback task. When inactive, the
//! forwarding path short-circuits on an atomic flag so the hot player loop
//! pays no lock cost.
//!
//! The bridge owns its own statics here rather than being exposed as a
//! `Mutex<Option<Sender>>` in `ui/flush.rs`: that way `ui/` does not need
//! to know anything about `BridgeEvent`, and external code touches the
//! bridge only through `set_active` / `forward`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::mpsc;

use crate::connect::receiver::speaker_bridge::BridgeEvent;
use crate::connect::types::PlayerState as ConnectPlayerState;
use crate::player::{PlaybackState, PlayerEvent};

/// Monotonic counter stamped on each bridge event. The receiver drops
/// events whose generation does not match the current media, preventing
/// stale events from a previous track from affecting the new one after
/// rapid skips.
pub(crate) static ENGINE_GEN: AtomicU64 = AtomicU64::new(0);

/// Fast check to skip the mutex lock when no receiver is active. Kept
/// separate from `BRIDGE_TX` so the common "receiver inactive" path only
/// reads an atomic.
static BRIDGE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Sender for Connect bridge events. `Some` iff the receiver is active.
static BRIDGE_TX: Mutex<Option<mpsc::Sender<BridgeEvent>>> = Mutex::new(None);

/// Install or clear the bridge sender. Called by `ConnectManager` when it
/// starts or stops the receiver, and by `main` during app shutdown.
pub(crate) fn set_active(tx: Option<mpsc::Sender<BridgeEvent>>) {
    let active = tx.is_some();
    *BRIDGE_TX.lock().unwrap() = tx;
    BRIDGE_ACTIVE.store(active, Ordering::Release);
}

/// Forward a player event to the receiver, if active. Called from
/// `ui::flush`'s per-event hook on the player event loop.
pub(crate) fn forward(event: &PlayerEvent) {
    if !BRIDGE_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    if matches!(event, PlayerEvent::TimeUpdate(..)) {
        // Log once to confirm the bridge forwarding path is live.
        static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::Relaxed) {
            crate::vprintln!("[connect::bridge] First TimeUpdate forwarded to receiver");
        }
    }
    let guard = match BRIDGE_TX.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let tx = match guard.as_ref() {
        Some(tx) => tx,
        None => return,
    };

    let engine_gen = ENGINE_GEN.load(Ordering::Relaxed);

    let bridge_event = match event {
        PlayerEvent::StateChange(state, _seq) => match state {
            PlaybackState::Ready => Some(BridgeEvent::Prepared { engine_gen }),
            PlaybackState::Active => Some(BridgeEvent::StatusUpdated {
                state: ConnectPlayerState::Playing,
                engine_gen,
            }),
            PlaybackState::Paused => Some(BridgeEvent::StatusUpdated {
                state: ConnectPlayerState::Paused,
                engine_gen,
            }),
            PlaybackState::Idle => Some(BridgeEvent::StatusUpdated {
                state: ConnectPlayerState::Buffering,
                engine_gen,
            }),
            PlaybackState::Completed => Some(BridgeEvent::PlaybackCompleted {
                has_next_media: false,
                engine_gen,
            }),
            PlaybackState::Stopped => Some(BridgeEvent::StatusUpdated {
                state: ConnectPlayerState::Idle,
                engine_gen,
            }),
            _ => None,
        },
        PlayerEvent::TimeUpdate(seconds, _seq) => {
            let ms = (*seconds * 1000.0) as u64;
            Some(BridgeEvent::ProgressUpdated {
                progress_ms: ms,
                duration_ms: 0,
                engine_gen,
            })
        }
        PlayerEvent::MediaError { error, .. } => Some(BridgeEvent::PlaybackError {
            status_code: error.clone(),
            engine_gen,
        }),
        _ => None,
    };

    if let Some(evt) = bridge_event {
        let _ = tx.try_send(evt);
    }
}
