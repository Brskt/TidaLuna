//! State snapshot query. Single source of truth for
//! `connect.get_state`: callers go through `with_state` -> `ConnectManager`.

use crate::app_state::{IpcCallback, with_state};

pub(super) fn get_state(callback: IpcCallback) {
    let snapshot = with_state(|state| state.connect.as_ref().map(|cm| cm.get_state_snapshot()))
        .flatten()
        .unwrap_or(serde_json::json!({}));
    callback
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .success_str(&serde_json::to_string(&snapshot).unwrap_or_default());
}
