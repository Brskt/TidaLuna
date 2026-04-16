//! IPC handlers for queue-related operations: `loadMediaInfo`,
//! `loadCloudQueue`, `selectQueueItem`, and `updateServerInfo`
//! (quality update). Each builds the wire command and dispatches.

use crate::app_state::{IpcCallback, IpcMessage};

use super::helpers::{get_server_infos, send_device_cmd};

pub(super) fn load_media(msg: &IpcMessage, callback: IpcCallback) {
    let media_info = msg.args.first().cloned().unwrap_or_default();
    send_device_cmd(
        serde_json::json!({"command": "loadMediaInfo", "mediaInfo": media_info}),
        callback,
    );
}

pub(super) fn load_queue(msg: &IpcMessage, callback: IpcCallback) {
    let data = msg.args.first().cloned().unwrap_or_default();
    let quality = data
        .get("audioquality")
        .and_then(|v| v.as_str())
        .unwrap_or("HIGH");
    let Some((content_si, queue_si)) = get_server_infos(quality) else {
        callback.lock().unwrap().failure(500, "No controller");
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
    let quality = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("HIGH")
        .to_string();
    let Some((content_si, queue_si)) = get_server_infos(&quality) else {
        callback.lock().unwrap().failure(500, "No controller");
        return;
    };

    send_device_cmd(
        serde_json::json!({
            "command": "updateServerInfo",
            "contentServerInfo": content_si,
            "queueServerInfo": queue_si,
        }),
        callback,
    );
}
