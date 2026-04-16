//! IPC handlers for playback control (play/pause, seek, volume, mute,
//! next/previous, repeat mode, shuffle). Each handler builds a small JSON
//! command and delegates to `send_device_cmd`.

use crate::app_state::{IpcCallback, IpcMessage};
use crate::connect::types::PlayerState;

use super::helpers::{get_last_player_state, send_device_cmd, send_device_cmd_simple};

pub(super) fn play_or_pause(callback: IpcCallback) {
    let cmd = match get_last_player_state() {
        PlayerState::Playing => "pause",
        _ => "play",
    };
    send_device_cmd_simple(cmd, callback);
}

pub(super) fn play_next(callback: IpcCallback) {
    send_device_cmd_simple("next", callback);
}

pub(super) fn play_previous(callback: IpcCallback) {
    send_device_cmd_simple("previous", callback);
}

pub(super) fn refresh_queue(callback: IpcCallback) {
    send_device_cmd_simple("refreshQueue", callback);
}

pub(super) fn seek(msg: &IpcMessage, callback: IpcCallback) {
    let position = msg.args.first().and_then(|v| v.as_u64()).unwrap_or(0);
    send_device_cmd(
        serde_json::json!({"command": "seek", "position": position}),
        callback,
    );
}

pub(super) fn set_volume(msg: &IpcMessage, callback: IpcCallback) {
    let level = msg.args.first().and_then(|v| v.as_f64()).unwrap_or(0.0);
    send_device_cmd(
        serde_json::json!({"command": "setVolume", "level": level}),
        callback,
    );
}

pub(super) fn set_mute(msg: &IpcMessage, callback: IpcCallback) {
    let mute = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(false);
    send_device_cmd(
        serde_json::json!({"command": "setMute", "mute": mute}),
        callback,
    );
}

pub(super) fn set_repeat(msg: &IpcMessage, callback: IpcCallback) {
    let mode = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("NONE")
        .to_string();
    send_device_cmd(
        serde_json::json!({"command": "setRepeatMode", "repeatMode": mode}),
        callback,
    );
}

pub(super) fn set_shuffle(msg: &IpcMessage, callback: IpcCallback) {
    let shuffle = msg.args.first().and_then(|v| v.as_bool()).unwrap_or(false);
    send_device_cmd(
        serde_json::json!({"command": "setShuffle", "shuffle": shuffle}),
        callback,
    );
}
