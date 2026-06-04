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

/// Whether a Connect controller is currently connected. While false the
/// bridge is idle: local player events are not forwarded to the receiver at
/// all, so the whole notify/broadcast chain stays dormant until someone
/// connects. Toggled by the routing loop on the 0<->1 client edge.
static BRIDGE_HAS_CLIENT: AtomicBool = AtomicBool::new(false);

/// Sender for Connect bridge events. `Some` iff the receiver is active.
static BRIDGE_TX: Mutex<Option<mpsc::Sender<BridgeEvent>>> = Mutex::new(None);

/// Install or clear the bridge sender. Called by `ConnectManager` when it
/// starts or stops the receiver, and by `main` during app shutdown.
pub(crate) fn set_active(tx: Option<mpsc::Sender<BridgeEvent>>) {
    let active = tx.is_some();
    *BRIDGE_TX.lock().unwrap_or_else(|e| e.into_inner()) = tx;
    BRIDGE_ACTIVE.store(active, Ordering::Release);
    // BRIDGE_HAS_CLIENT is owned solely by the receiver routing loop, which resets
    // it on start and toggles it on client connect/disconnect. set_active must not
    // write it, or it could clobber a connect that races receiver startup. When
    // inactive, forward() short-circuits on BRIDGE_ACTIVE regardless.
}

/// Toggle whether a Connect controller is connected. While no client is
/// connected `forward` short-circuits, leaving the receiver fully idle so
/// local playback never drives its notify/broadcast chain.
pub(crate) fn set_client_connected(connected: bool) {
    BRIDGE_HAS_CLIENT.store(connected, Ordering::Release);
}

/// Forward a player event to the receiver, if active. Called from
/// `ui::flush`'s per-event hook on the player event loop.
pub(crate) fn forward(event: &PlayerEvent) {
    // Idle unless the receiver is running AND a controller is connected: with
    // nobody to notify, forwarding/notify/broadcast is wasted work. Resumes
    // the instant a client connects.
    if !BRIDGE_ACTIVE.load(Ordering::Acquire) || !BRIDGE_HAS_CLIENT.load(Ordering::Acquire) {
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
