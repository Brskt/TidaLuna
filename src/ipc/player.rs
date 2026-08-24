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

/// The player thread is the only receiver and nothing restarts it: a refused send means this
/// command and every later one is gone. These handlers have no reply channel to the renderer
/// (`sendIpc` attaches no id, and `player.*` is not in the callback-aware dispatch), which is
/// exactly why the log has to be ungated: it is the only place the loss can surface.
fn report_undelivered(command: &str, sent: anyhow::Result<()>) {
    if let Err(e) = sent {
        crate::verr!("[IPC]    player.{command} not delivered: {e}");
    }
}

pub(crate) fn handle_ipc_message(request: &str) {
    let msg: IpcMessage = match serde_json::from_str(request) {
        Ok(m) => m,
        Err(e) => {
            // The failure, never the envelope: it is renderer-supplied and may carry a capability,
            // which must not reach the log. Serde names the offending field and position without
            // echoing a well-formed one (`cap` only appears in the message when it is not a string,
            // which no issued capability is).
            crate::vprintln!("Received an IPC message that does not parse: {e}");
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
            crate::ui::log_safe_channel(&msg.channel)
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

/// Whether a measured length may be published under an announced track's name. Two ids match
/// or nothing does: an unidentified payload is not evidence of sameness.
pub(crate) fn same_track(measured: Option<&str>, announced: Option<&str>) -> bool {
    matches!((measured, announced), (Some(a), Some(b)) if a == b)
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
                        product_id,
                        restart,
                        want_play,
                    } => {
                        if let Err(e) = state
                            .player
                            .load(url, format, key, product_id, restart, want_play)
                        {
                            crate::vprintln!("[PLAYER] Failed to load track: {}", e);
                        }
                        PlayerIpcEffects::default()
                    }
                    PlayerIpc::LoadDash {
                        init_url,
                        segment_urls,
                        format,
                        product_id,
                    } => {
                        if let Err(e) =
                            state
                                .player
                                .load_dash(init_url, segment_urls, format, product_id)
                        {
                            crate::vprintln!("[PLAYER] Failed to load DASH track: {}", e);
                        }
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
                    PlayerIpc::Preload {
                        url,
                        format,
                        key,
                        product_id,
                    } => {
                        // The renderer names the track off the play queue; the SDK's own
                        // delegate carries only the url triple. A tag that names the wrong
                        // track fails closed: `same_track` refuses it downstream and nothing
                        // is published, which is exactly what an absent tag already does.
                        let track = TrackInfo {
                            url,
                            format,
                            key,
                            product_id,
                        };
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
                        let (title, artist) = (meta.title.clone(), meta.artist.clone());
                        let mc = state.media_controls.take();
                        // The measurement's own tag vouches for the length; the shared
                        // metadata slot cannot, being rewritten by a track's first frame.
                        // Nothing is cleared here either: a measurement can land before its
                        // own metadata, and no `Duration` event is owed to replace it.
                        let length = state
                            .media_duration
                            .as_ref()
                            .filter(|d| same_track(d.track_id.as_deref(), meta.id.as_deref()))
                            .map(|d| d.secs);
                        match crate::state::CURRENT_METADATA.lock() {
                            Ok(mut lock) => *lock = Some(meta),
                            Err(e) => {
                                crate::vprintln!("[PLAYER] CURRENT_METADATA lock poisoned: {e}")
                            }
                        }
                        let mc_metadata = Some((title, artist, length));
                        PlayerIpcEffects {
                            mc,
                            mc_metadata,
                            ..Default::default()
                        }
                    }
                    PlayerIpc::Play => {
                        report_undelivered("play", state.player.play());
                        PlayerIpcEffects::default()
                    }
                    PlayerIpc::Pause => {
                        report_undelivered("pause", state.player.pause());
                        PlayerIpcEffects::default()
                    }
                    PlayerIpc::Stop => {
                        report_undelivered("stop", state.player.stop());
                        PlayerIpcEffects::default()
                    }
                    PlayerIpc::Seek { time } => {
                        let seq =
                            crate::player::LOAD_SEQ.load(std::sync::atomic::Ordering::Relaxed);
                        state.pending_time_update = Some((time, seq));
                        let batch = take_flush_batch(state);
                        report_undelivered("seek", state.player.seek(time));
                        PlayerIpcEffects {
                            batch: Some(batch),
                            ..Default::default()
                        }
                    }
                    PlayerIpc::Volume { volume } => {
                        report_undelivered("volume", state.player.set_volume(volume));
                        PlayerIpcEffects::default()
                    }
                    PlayerIpc::DevicesGet { request_id } => {
                        report_undelivered(
                            "devices.get",
                            state.player.get_audio_devices(request_id),
                        );
                        PlayerIpcEffects::default()
                    }
                    PlayerIpc::DevicesSet { id, mode } => {
                        report_undelivered("devices.set", state.player.set_audio_device(id, mode));
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
#[path = "../../tests/unit/ipc/player.rs"]
mod tests;
