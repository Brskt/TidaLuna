//! IPC handlers for queue-related operations: `loadMediaInfo`,
//! `loadCloudQueue`, `selectQueueItem`, and `updateServerInfo`
//! (quality update). Each builds the wire command and dispatches.

use crate::app_state::{IpcCallback, IpcMessage};
use crate::connect::controller::server_info::ServerInfoClaim;

use super::helpers::{answer_queued, prepare_load, prepare_update, send_device_cmd};

pub(super) fn load_media(msg: &IpcMessage, callback: IpcCallback) {
    let media_info = msg.args.first().cloned().unwrap_or_default();
    send_device_cmd(
        serde_json::json!({"command": "loadMediaInfo", "mediaInfo": media_info}),
        callback,
    );
}

/// A quality TIDAL names: text, not empty. Anything else names none, and none is remembered.
fn named_quality(value: Option<&serde_json::Value>) -> Option<&str> {
    value.and_then(|v| v.as_str()).filter(|q| !q.is_empty())
}

pub(super) fn load_queue(msg: &IpcMessage, callback: IpcCallback) {
    let data = msg.args.first().cloned().unwrap_or_default();
    let named = named_quality(data.get("audioquality"));
    let Some((content_si, queue_si)) = prepare_load(named, named.unwrap_or("HIGH")) else {
        callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failure(500, "Not signed in");
        return;
    };

    let cmd = serde_json::json!({
        "command": "loadCloudQueue",
        "autoplay": data.get("autoplay").and_then(|v| v.as_bool()).unwrap_or(false),
        "position": data.get("position").and_then(|v| v.as_u64()).unwrap_or(0),
        "currentMediaInfo": data.get("currentMediaInfo"),
        "queueInfo": {
            "queueId": data.get("queueId").and_then(|v| v.as_str()).unwrap_or(""),
            "repeatMode": data.get("repeatMode").and_then(|v| v.as_str()).unwrap_or("NONE"),
            "shuffled": data.get("shuffled").and_then(|v| v.as_bool()).unwrap_or(false),
            "maxAfterSize": data.get("maxAfterSize").and_then(|v| v.as_u64()).unwrap_or(10),
            "maxBeforeSize": data.get("maxBeforeSize").and_then(|v| v.as_u64()).unwrap_or(10),
        },
        "contentServerInfo": content_si,
        "queueServerInfo": queue_si,
    });
    send_device_cmd(cmd, callback);
}

pub(super) fn select_queue_item(msg: &IpcMessage, callback: IpcCallback) {
    let media_info = msg.args.first().cloned().unwrap_or_default();
    send_device_cmd(
        serde_json::json!({"command": "selectQueueItem", "mediaInfo": media_info}),
        callback,
    );
}

pub(super) fn update_quality(msg: &IpcMessage, callback: IpcCallback) {
    let Some(handout) = prepare_update(named_quality(msg.args.first())) else {
        callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failure(500, "Not signed in");
        return;
    };

    match handout.due {
        // The device holds this very content already: nothing is sent, and the answer is a bare ok.
        ServerInfoClaim::Held => callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .success_str("\"ok\""),
        ServerInfoClaim::NoSession => send_device_cmd(handout.command, callback),
        ServerInfoClaim::Queued(queued) => answer_queued(queued, callback),
        ServerInfoClaim::Failed(e) => callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failure(500, &e),
    }
}
