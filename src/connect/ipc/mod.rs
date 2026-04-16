//! Top-level IPC dispatch for `connect.*` channels.
//!
//! Every inbound message is routed to a sub-module by domain
//! (`controller`, `playback`, `queue`, `receiver`, `state`) so the bodies
//! stay small and reviewable. Shared concerns (device-command dispatch,
//! WS event forwarding, CEF posting) live in `helpers`.

mod controller;
mod helpers;
mod playback;
mod queue;
mod receiver;
mod state;

pub(crate) use helpers::{post_emit, post_emit_with_data};

use crate::app_state::{IpcCallback, IpcMessage};

/// Fire-and-forget IPC (no callback).
pub(crate) fn handle_connect_ipc(msg: &IpcMessage) {
    let sub = msg.channel.strip_prefix("connect.").unwrap_or("");
    crate::vprintln!("[connect::ipc] {}", sub);

    match sub {
        "controller.initialize" => controller::initialize(msg),
        "controller.discover" => controller::discover(),
        "controller.refresh" => controller::refresh(),
        "controller.set_auth" => controller::set_auth(msg),
        "receiver.start" => receiver::start(),
        "receiver.stop" => receiver::stop(),
        "receiver.set_always_on" => receiver::set_always_on(msg),
        _ => {
            crate::vprintln!("[connect::ipc] Unknown fire-and-forget: connect.{}", sub);
        }
    }
}

/// Invoke IPC (with callback).
pub(crate) fn handle_connect_invoke(msg: IpcMessage, callback: IpcCallback) {
    let sub = msg
        .channel
        .strip_prefix("connect.")
        .unwrap_or("")
        .to_string();

    match sub.as_str() {
        "get_state" => state::get_state(callback),

        "controller.connect" => controller::connect(msg, callback),
        "controller.disconnect" => controller::disconnect(msg, callback),

        "controller.play_or_pause" => playback::play_or_pause(callback),
        "controller.play_next" => playback::play_next(callback),
        "controller.play_previous" => playback::play_previous(callback),
        "controller.refresh_queue" => playback::refresh_queue(callback),
        "controller.seek" => playback::seek(&msg, callback),
        "controller.set_volume" => playback::set_volume(&msg, callback),
        "controller.set_mute" => playback::set_mute(&msg, callback),
        "controller.set_repeat" => playback::set_repeat(&msg, callback),
        "controller.set_shuffle" => playback::set_shuffle(&msg, callback),

        "controller.load_media" => queue::load_media(&msg, callback),
        "controller.load_queue" => queue::load_queue(&msg, callback),
        "controller.select_queue_item" => queue::select_queue_item(&msg, callback),
        "controller.update_quality" => queue::update_quality(&msg, callback),

        _ => {
            // Fallback: try fire-and-forget path.
            handle_connect_ipc(&msg);
            callback.lock().unwrap().success_str("ok");
        }
    }
}
