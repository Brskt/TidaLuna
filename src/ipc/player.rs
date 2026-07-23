use crate::app_state::{IpcMessage, with_state};
use crate::ipc::plugin::handle_jsrt_fire_and_forget;
use crate::ipc::window::handle_window_ipc;
use crate::player::ipc::{PlayerIpc, parse_player_ipc};
use crate::state::TrackInfo;
use crate::ui::flush::{FlushBatch, run_flush_batch, take_flush_batch};

/// Channels whose args may carry secrets - logged by name only. Default-redacts every
/// privileged channel (so a new one can't leak by omission) plus signed media-load URLs.
fn should_redact_args(channel: &str) -> bool {
    crate::ui::is_privileged_channel(channel)
        || matches!(
            channel,
            "player.load" | "player.load_dash" | "player.recover" | "player.preload"
        )
}

pub(crate) fn handle_ipc_message(request: &str) {
    let msg: IpcMessage = match serde_json::from_str(request) {
        Ok(m) => m,
        Err(_) => {
            crate::vprintln!("Received unknown IPC message: {}", request);
            return;
        }
    };

    if msg.channel == "player.dbg" {
        // Diagnostic-only; JS gates emission on the log level before it reaches here.
        crate::vprintln2!("[JS-DBG] {:?}", msg.args);
        return;
    }

    // Args may carry secrets (tokens, signed URLs, AES keys): log the channel name
    // only - they must never hit the persistent sink, and truncation isn't redaction.
    let redacted_args = should_redact_args(&msg.channel);
    if redacted_args {
        crate::vprintln!(
            "IPC Message: IpcMessage {{ channel: {:?}, args: [<redacted>] }}",
            msg.channel
        );
    } else {
        crate::vprintln!("IPC Message: {}", msg);
    }

    if msg.channel.starts_with("player.") {
        handle_player_ipc(&msg);
    } else if msg.channel.starts_with("connect.") {
        crate::connect::ipc::handle_connect_ipc(&msg);
    } else if msg.channel.starts_with("jsrt.") {
        handle_jsrt_fire_and_forget(&msg);
    } else {
        handle_window_ipc(&msg);
    }
}

#[derive(Default)]
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
                    PlayerIpc::Load {
                        url,
                        format,
                        key,
                        restart,
                        want_play,
                    } => {
                        if let Err(e) = state.player.load(url, format, key, restart, want_play) {
                            crate::vprintln!("[PLAYER] Failed to load track: {}", e);
                        }
                        crate::memory_pressure::purge_image_cache();
                        PlayerIpcEffects::default()
                    }
                    PlayerIpc::LoadDash {
                        init_url,
                        segment_urls,
                        format,
                    } => {
                        if let Err(e) = state.player.load_dash(init_url, segment_urls, format) {
                            crate::vprintln!("[PLAYER] Failed to load DASH track: {}", e);
                        }
                        crate::memory_pressure::purge_image_cache();
                        PlayerIpcEffects::default()
                    }
                    PlayerIpc::Recover {
                        url,
                        format,
                        key,
                        target_time,
                    } => {
                        if let Err(e) = state.player.recover(url, format, key, target_time) {
                            crate::vprintln!("[PLAYER] Failed to recover track: {}", e);
                        }
                        PlayerIpcEffects::default()
                    }
                    PlayerIpc::Preload { url, format, key } => {
                        let track = TrackInfo { url, format, key };
                        crate::state::rt_handle().spawn(async move {
                            crate::audio::preload::start_preload(track).await;
                        });
                        PlayerIpcEffects::default()
                    }
                    PlayerIpc::PreloadCancel => {
                        crate::state::rt_handle().spawn(async {
                            crate::audio::preload::cancel_preload().await;
                        });
                        PlayerIpcEffects::default()
                    }
                    PlayerIpc::Metadata { payload } => {
                        let meta = crate::util::metadata::parse_track_metadata(&payload);
                        let duration = state.media_duration;
                        let mc = state.media_controls.take();
                        let mc_metadata = Some((meta.title.clone(), meta.artist.clone(), duration));
                        match crate::state::CURRENT_METADATA.lock() {
                            Ok(mut lock) => *lock = Some(meta),
                            Err(e) => {
                                crate::vprintln!("[PLAYER] CURRENT_METADATA lock poisoned: {e}")
                            }
                        }
                        PlayerIpcEffects {
                            mc,
                            mc_metadata,
                            ..Default::default()
                        }
                    }
                    PlayerIpc::Play => {
                        let _ = state.player.play();
                        PlayerIpcEffects::default()
                    }
                    PlayerIpc::Pause => {
                        let _ = state.player.pause();
                        PlayerIpcEffects::default()
                    }
                    PlayerIpc::Stop => {
                        let _ = state.player.stop();
                        PlayerIpcEffects::default()
                    }
                    PlayerIpc::Seek { time } => {
                        let seq =
                            crate::player::LOAD_SEQ.load(std::sync::atomic::Ordering::Relaxed);
                        state.pending_time_update = Some((time, seq));
                        let batch = take_flush_batch(state);
                        let _ = state.player.seek(time);
                        PlayerIpcEffects {
                            batch: Some(batch),
                            ..Default::default()
                        }
                    }
                    PlayerIpc::Volume { volume } => {
                        let _ = state.player.set_volume(volume);
                        PlayerIpcEffects::default()
                    }
                    PlayerIpc::DevicesGet { request_id } => {
                        let _ = state.player.get_audio_devices(request_id);
                        PlayerIpcEffects::default()
                    }
                    PlayerIpc::DevicesSet { id, mode } => {
                        let _ = state.player.set_audio_device(id, mode);
                        PlayerIpcEffects::default()
                    }
                },
                Err(e) => {
                    crate::vprintln!("[IPC]    Invalid player IPC ({}): {:?}", msg.channel, e);
                    PlayerIpcEffects::default()
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

#[cfg(test)]
mod tests {
    use super::should_redact_args;

    #[test]
    fn redacts_known_secret_channels() {
        assert!(should_redact_args("jsrt.set_token"));
        assert!(should_redact_args("connect.controller.set_auth"));
        assert!(should_redact_args("player.load"));
        assert!(should_redact_args("player.load_dash"));
    }

    #[test]
    fn redacts_privileged_channels_by_default() {
        // A privileged channel not in any explicit list must still be redacted, so
        // a secret-bearing one added later can't leak its args by omission.
        assert!(should_redact_args("jsrt.session_clear"));
        assert!(should_redact_args("settings.set_log_level"));
        assert!(should_redact_args("updater.apply"));
    }

    #[test]
    fn keeps_benign_channels_visible() {
        assert!(!should_redact_args("player.play"));
        assert!(!should_redact_args("web.loaded"));
        assert!(!should_redact_args("menu.clicked"));
    }
}
