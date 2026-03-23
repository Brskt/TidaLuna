use crate::app_state::{IpcMessage, with_state};
use crate::ipc::plugin::handle_jsrt_fire_and_forget;
use crate::ipc::window::handle_window_ipc;
use crate::player::ipc::{PlayerIpc, parse_player_ipc};
use crate::state::TrackInfo;
use crate::ui::flush::{FlushBatch, run_flush_batch, take_flush_batch};

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

struct PlayerIpcEffects {
    batch: Option<FlushBatch>,
    mc: Option<crate::platform::media_controls::OsMediaControls>,
    mc_metadata: Option<(String, String, Option<f64>)>,
}

fn handle_player_ipc(msg: &IpcMessage) {
    let effects =
        with_state(
            |state| match parse_player_ipc(&msg.channel, &msg.args, msg.id.as_deref()) {
                Ok(player_ipc) => match player_ipc {
                    PlayerIpc::Load { url, format, key } => {
                        if let Err(e) = state.player.load(url, format, key) {
                            eprintln!("Failed to load track: {}", e);
                        }
                        PlayerIpcEffects {
                            batch: None,
                            mc: None,
                            mc_metadata: None,
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
                        PlayerIpcEffects {
                            batch: None,
                            mc: None,
                            mc_metadata: None,
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
                        PlayerIpcEffects {
                            batch: None,
                            mc: None,
                            mc_metadata: None,
                        }
                    }
                    PlayerIpc::Preload { url, format, key } => {
                        let track = TrackInfo { url, format, key };
                        state.rt_handle.spawn(async move {
                            crate::audio::preload::start_preload(track).await;
                        });
                        PlayerIpcEffects {
                            batch: None,
                            mc: None,
                            mc_metadata: None,
                        }
                    }
                    PlayerIpc::PreloadCancel => {
                        state.rt_handle.spawn(async {
                            crate::audio::preload::cancel_preload().await;
                        });
                        PlayerIpcEffects {
                            batch: None,
                            mc: None,
                            mc_metadata: None,
                        }
                    }
                    PlayerIpc::Metadata { payload } => {
                        let meta = crate::util::metadata::parse_track_metadata(&payload);
                        let duration = state.media_duration;
                        let mc = state.media_controls.take();
                        let mc_metadata = Some((meta.title.clone(), meta.artist.clone(), duration));
                        let mut lock = crate::state::CURRENT_METADATA
                            .lock()
                            .expect("CURRENT_METADATA lock poisoned");
                        *lock = Some(meta);
                        PlayerIpcEffects {
                            batch: None,
                            mc,
                            mc_metadata,
                        }
                    }
                    PlayerIpc::Play => {
                        let _ = state.player.play();
                        PlayerIpcEffects {
                            batch: None,
                            mc: None,
                            mc_metadata: None,
                        }
                    }
                    PlayerIpc::Pause => {
                        let _ = state.player.pause();
                        PlayerIpcEffects {
                            batch: None,
                            mc: None,
                            mc_metadata: None,
                        }
                    }
                    PlayerIpc::Stop => {
                        let _ = state.player.stop();
                        PlayerIpcEffects {
                            batch: None,
                            mc: None,
                            mc_metadata: None,
                        }
                    }
                    PlayerIpc::Seek { time } => {
                        let seq =
                            crate::player::LOAD_SEQ.load(std::sync::atomic::Ordering::Relaxed);
                        state.pending_time_update = Some((time, seq));
                        let batch = take_flush_batch(state);
                        let _ = state.player.seek(time);
                        PlayerIpcEffects {
                            batch: Some(batch),
                            mc: None,
                            mc_metadata: None,
                        }
                    }
                    PlayerIpc::Volume { volume } => {
                        let _ = state.player.set_volume(volume);
                        PlayerIpcEffects {
                            batch: None,
                            mc: None,
                            mc_metadata: None,
                        }
                    }
                    PlayerIpc::DevicesGet { request_id } => {
                        let _ = state.player.get_audio_devices(request_id);
                        PlayerIpcEffects {
                            batch: None,
                            mc: None,
                            mc_metadata: None,
                        }
                    }
                    PlayerIpc::DevicesSet { id, exclusive } => {
                        let _ = state.player.set_audio_device(id, exclusive);
                        PlayerIpcEffects {
                            batch: None,
                            mc: None,
                            mc_metadata: None,
                        }
                    }
                },
                Err(e) => {
                    crate::vprintln!("[IPC]    Invalid player IPC ({}): {:?}", msg.channel, e);
                    PlayerIpcEffects {
                        batch: None,
                        mc: None,
                        mc_metadata: None,
                    }
                }
            },
        );

    if let Some(mut effects) = effects {
        if let (Some(mc), Some((title, artist, duration))) = (&mut effects.mc, &effects.mc_metadata)
        {
            mc.set_metadata(title, artist, *duration);
        }

        if effects.mc.is_some() {
            with_state(|state| {
                state.media_controls = effects.mc.take();
            });
        }

        if let Some(batch) = effects.batch {
            run_flush_batch(batch);
        }
    }
}
