use crate::app_state::{IpcMessage, with_state};
use crate::ipc::plugin::handle_jsrt_fire_and_forget;
use crate::ipc::window::handle_window_ipc;
use crate::player::ipc::{PlayerIpc, parse_player_ipc};
use crate::state::TrackInfo;
use crate::ui::flush::flush_bridge_now;

pub(crate) fn handle_ipc_message(request: &str) {
    let msg: IpcMessage = match serde_json::from_str(request) {
        Ok(m) => m,
        Err(_) => {
            crate::vprintln!("Received unknown IPC message: {}", request);
            return;
        }
    };

    if msg.channel == "player.dbg" {
        let dominated_by_time = msg
            .args
            .first()
            .and_then(|v| v.as_str())
            .is_some_and(|s| s == "setCurrentTime");
        if !dominated_by_time {
            crate::vprintln!("[JS-DBG] {:?}", msg.args);
        }
        return;
    }

    crate::vprintln!("IPC Message: {}", msg);

    if msg.channel.starts_with("player.") {
        handle_player_ipc(&msg);
    } else if msg.channel.starts_with("jsrt.") {
        handle_jsrt_fire_and_forget(&msg);
    } else {
        handle_window_ipc(&msg);
    }
}

fn handle_player_ipc(msg: &IpcMessage) {
    with_state(
        |state| match parse_player_ipc(&msg.channel, &msg.args, msg.id.as_deref()) {
            Ok(player_ipc) => match player_ipc {
                PlayerIpc::Load { url, format, key } => {
                    if let Err(e) = state.player.load(url, format, key) {
                        eprintln!("Failed to load track: {}", e);
                    }
                }
                PlayerIpc::LoadDash {
                    init_url,
                    segment_urls,
                    format,
                } => {
                    if let Err(e) = state.player.load_dash(init_url, segment_urls, format) {
                        eprintln!("Failed to load DASH track: {}", e);
                    }
                }
                PlayerIpc::Recover {
                    url,
                    format,
                    key,
                    target_time,
                } => {
                    if let Err(e) = state.player.recover(url, format, key, target_time) {
                        eprintln!("Failed to recover track: {}", e);
                    }
                }
                PlayerIpc::Preload { url, format, key } => {
                    let track = TrackInfo { url, format, key };
                    state.rt_handle.spawn(async move {
                        crate::audio::preload::start_preload(track).await;
                    });
                }
                PlayerIpc::PreloadCancel => {
                    state.rt_handle.spawn(async {
                        crate::audio::preload::cancel_preload().await;
                    });
                }
                PlayerIpc::Metadata { payload } => {
                    let meta = crate::util::metadata::parse_track_metadata(&payload);
                    if let Some(ref mut mc) = state.media_controls {
                        mc.set_metadata(&meta.title, &meta.artist, state.media_duration);
                    }
                    let mut lock = crate::state::CURRENT_METADATA
                        .lock()
                        .expect("CURRENT_METADATA lock poisoned");
                    *lock = Some(meta);
                }
                PlayerIpc::Play => {
                    let _ = state.player.play();
                }
                PlayerIpc::Pause => {
                    let _ = state.player.pause();
                }
                PlayerIpc::Stop => {
                    let _ = state.player.stop();
                }
                PlayerIpc::Seek { time } => {
                    let seq = crate::player::LOAD_SEQ.load(std::sync::atomic::Ordering::Relaxed);
                    state.pending_time_update = Some((time, seq));
                    flush_bridge_now(state);
                    let _ = state.player.seek(time);
                }
                PlayerIpc::Volume { volume } => {
                    let _ = state.player.set_volume(volume);
                }
                PlayerIpc::DevicesGet { request_id } => {
                    let _ = state.player.get_audio_devices(request_id);
                }
                PlayerIpc::DevicesSet { id, exclusive } => {
                    let _ = state.player.set_audio_device(id, exclusive);
                }
            },
            Err(e) => {
                crate::vprintln!("[IPC]    Invalid player IPC ({}): {:?}", msg.channel, e);
            }
        },
    );
}
