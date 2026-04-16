//! IPC handlers for receiver-side lifecycle: start and stop.

use crate::app_state::{IpcMessage, with_state};
use crate::connect::types::ReceiverConfig;

pub(super) fn start() {
    let config = ReceiverConfig::default();
    let Some(rt) = crate::state::RT_HANDLE.get() else {
        return;
    };
    rt.spawn(async move {
        let mut cm = with_state(|state| state.connect.take()).flatten();
        if let Some(ref mut cm) = cm
            && let Err(e) = cm.start_receiver(config).await
        {
            crate::vprintln!("[connect::ipc] Receiver start failed: {}", e);
        }
        with_state(|state| {
            state.connect = cm;
        });
    });
}

pub(super) fn stop() {
    let Some(rt) = crate::state::RT_HANDLE.get() else {
        return;
    };
    rt.spawn(async {
        let mut cm = with_state(|state| state.connect.take()).flatten();
        if let Some(ref mut cm) = cm {
            cm.stop_receiver().await;
        }
        with_state(|state| {
            state.connect = cm;
        });
    });
}

pub(super) fn set_always_on(msg: &IpcMessage) {
    let enabled = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(true);
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
