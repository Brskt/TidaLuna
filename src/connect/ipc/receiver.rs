//! IPC handlers for receiver-side lifecycle: start and stop.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::app_state::{IpcMessage, with_state};
use crate::connect::ConnectManager;
use crate::connect::receiver::SHUTDOWN_DEADLINE;
use crate::connect::types::ReceiverConfig;

/// Serializes receiver start/stop. Without it, two concurrent lifecycle calls
/// (TIDAL issues `discover` + `receiver.start` close together on login) could
/// both build a receiver - leaking one - or interleave a start with a stop.
static RECEIVER_LIFECYCLE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// In-memory mirror of `connect.receiver_always_on` (seeded at boot, updated by
/// `set_always_on`). The `start_receiver_task` gate reads it, making every start
/// path honor the toggle; an atomic, not a DB read, keeps that check race-free.
static RECEIVER_ENABLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_receiver_enabled(enabled: bool) {
    RECEIVER_ENABLED.store(enabled, Ordering::Relaxed);
}

/// False in a clone the TIDAL Connect server key never reached. The TLS acceptor takes
/// that key through `include_bytes!`, which no Rust condition can guard; `build.rs` probes
/// for the file and reports the result as a cfg.
const SERVER_KEY_BUNDLED: bool = cfg!(has_connect_server_key);

pub(super) fn start() {
    let Some(rt) = crate::state::RT_HANDLE.get() else {
        return;
    };
    rt.spawn(start_receiver_task(ReceiverConfig::default()));
}

/// Start the receiver if it isn't already running. The `ConnectManager` is
/// never moved out of `AppState`: the async build runs on owned data, then the
/// result is installed in a synchronous step; concurrent `connect.*` IPC
/// keeps seeing a live manager throughout.
pub(crate) async fn start_receiver_task(config: ReceiverConfig) {
    let _guard = RECEIVER_LIFECYCLE.lock().await;

    // Setting is authoritative on every path: a start while off is a no-op
    // (this is what closes the SDK remoteDesktop bypass).
    if !RECEIVER_ENABLED.load(Ordering::Relaxed) {
        return;
    }

    // Ungated: the TLS acceptor reports the same absence, but only through the gated log
    // below, which would leave a default config with no channel at all. Announced once per
    // process, `discover` spawning this task on every device-picker open and the verdict
    // being settled at build time. Checked after the setting, since a user who turned the
    // receiver off needs no word about it.
    if !SERVER_KEY_BUNDLED {
        static ANNOUNCED: AtomicBool = AtomicBool::new(false);
        if !ANNOUNCED.swap(true, Ordering::Relaxed) {
            crate::verr!(
                "[connect::ipc] Receiver unavailable: no TIDAL Connect server key was bundled at build time"
            );
        }
        return;
    }

    let active = with_state(|state| {
        state
            .connect
            .as_ref()
            .is_some_and(ConnectManager::is_receiver_active)
    })
    .unwrap_or(false);
    if active {
        return;
    }

    match ConnectManager::build_receiver(config).await {
        Ok((receiver, bridge_tx)) => {
            // Install under the lock. If the manager is gone (process shutting
            // down between the active-check and here), hand the receiver back so
            // it can be torn down gracefully rather than dropped - a bare drop
            // would skip the WS/mDNS shutdown and leak the bound socket.
            let orphan = with_state(|state| match state.connect.as_mut() {
                Some(cm) => {
                    cm.install_receiver(receiver, bridge_tx);
                    None
                }
                None => Some(receiver),
            })
            .flatten();
            if let Some(mut receiver) = orphan {
                crate::vprintln!("[connect::ipc] No manager to install receiver; shutting it down");
                receiver.shutdown(SHUTDOWN_DEADLINE).await;
            }
        }
        Err(e) => crate::vprintln!("[connect::ipc] Receiver start failed: {e}"),
    }
}

pub(super) fn stop() {
    let Some(rt) = crate::state::RT_HANDLE.get() else {
        return;
    };
    rt.spawn(async {
        let _guard = RECEIVER_LIFECYCLE.lock().await;
        let receiver = with_state(|state| {
            state
                .connect
                .as_mut()
                .and_then(ConnectManager::take_receiver)
        })
        .flatten();
        if let Some(mut receiver) = receiver {
            receiver.shutdown(SHUTDOWN_DEADLINE).await;
        }
    });
}

pub(super) fn set_always_on(msg: &IpcMessage) {
    let enabled = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(true);
    set_receiver_enabled(enabled);
    crate::state::db().call_settings(move |conn| {
        crate::settings::save_receiver_always_on(conn, enabled);
    });
    crate::vprintln!("[connect::ipc] Receiver always-on set to {}", enabled);
    if enabled {
        start();
    } else {
        stop();
    }
}
