//! State snapshot query. Single source of truth for
//! `connect.get_state`: callers go through `with_state` -> `ConnectManager`. Also
//! `connect.catch_up_level`, the page's catch-up with the output level.

use crate::app_state::{IpcCallback, with_state};

pub(super) fn get_state(callback: IpcCallback) {
    let snapshot = with_state(|state| {
        let mut snapshot = state
            .connect
            .as_ref()
            .map(|cm| cm.get_state_snapshot())
            .unwrap_or(serde_json::json!({}));
        // Read by TIDAL's page-load restore (`volume_restore.js`): it restores nothing over a
        // level someone chose.
        snapshot["outputLevel"] =
            serde_json::to_value(state.player.output_level().report()).unwrap_or_default();
        snapshot
    })
    .unwrap_or(serde_json::json!({}));
    callback
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .success_str(&serde_json::to_string(&snapshot).unwrap_or_default());
}

/// `connect.catch_up_level`: a page that loads takes in the level Rust plays
/// (`resyncOutputLevel`), and its later levels carry the count this report returns.
pub(super) fn catch_up_level(callback: IpcCallback) {
    let report = with_state(|state| state.player.catch_up_page().report())
        .and_then(|report| serde_json::to_string(&report).ok())
        .unwrap_or_else(|| "{}".to_string());
    callback
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .success_str(&report);
}
