use crate::app_state::{AppState, exec_js_on_frame, js_ipc_response, with_state};
use crate::bridge::PlayerBridgeEvent;
use crate::player::{PlaybackState, PlayerEvent};
use cef::*;

pub(crate) struct FlushBatch {
    browser: Option<Browser>,
    player_events: Vec<PlayerBridgeEvent>,
    misc_js: Vec<String>,
}

pub(crate) fn take_flush_batch(state: &mut AppState) -> FlushBatch {
    if let Some((time, seq)) = state.pending_time_update.take() {
        state
            .pending_player_events
            .push(PlayerBridgeEvent::time(time, seq));
    }

    FlushBatch {
        browser: state.browser.clone(),
        player_events: std::mem::take(&mut state.pending_player_events),
        misc_js: std::mem::take(&mut state.pending_misc_js),
    }
}

pub(crate) fn run_flush_batch(batch: FlushBatch) {
    let frame = batch.browser.as_ref().and_then(|b| b.main_frame());

    if !batch.player_events.is_empty()
        && let Ok(events_json) = serde_json::to_string(&batch.player_events)
    {
        let js = format!(
            "if (window.__TIDALUNAR_PLAYER_PUSH__) {{ window.__TIDALUNAR_PLAYER_PUSH__({}); }}",
            events_json
        );
        if let Some(ref frame) = frame {
            exec_js_on_frame(frame, &js);
        } else {
            crate::vprintln!(
                "[BRIDGE] flush DROPPED - {}",
                if batch.browser.is_none() {
                    "no browser"
                } else {
                    "no frame"
                }
            );
        }
    }

    if !batch.misc_js.is_empty() {
        let js_batch = batch.misc_js.join(";");
        if let Some(ref frame) = frame {
            exec_js_on_frame(frame, &js_batch);
        }
    }
}

struct PostLockEffects {
    batch: Option<FlushBatch>,
    should_schedule: bool,
    mc: Option<crate::platform::media_controls::OsMediaControls>,
    mc_action: MediaControlAction,
    #[cfg(target_os = "windows")]
    thumbbar: Option<crate::platform::thumbbar::ThumbBar>,
    #[cfg(target_os = "windows")]
    thumbbar_playing: Option<bool>,
}

enum MediaControlAction {
    None,
    SetPlayback(PlaybackState),
    SetMetadata {
        title: String,
        artist: String,
        duration: Option<f64>,
    },
}

/// What a measured length becomes on arrival: the tag is the measurement's own id, whatever
/// the shared slot happens to say by the time this runs, and the length goes out beside a name
/// only while that slot still names the same track. Reading the slot for the tag was the defect
/// this separates out: the slot answers "which track is announced", never "which track was
/// measured", and the two differ for as long as a load takes to reach the decoder.
///
/// A mismatch publishes nothing rather than publishing with no length: `set_metadata` replaces
/// the whole metadata object, so an empty duration erases one the announced track may already
/// have had. The announced-metadata handler publishes instead, when its own frame lands.
///
/// The measurement returned here is only offered to the slot; `record_measured_duration` owns
/// whether it lands, and turns an untagged one away.
fn settle_measured_duration(
    duration: f64,
    measured: Option<String>,
    announced: Option<&crate::state::TrackMetadata>,
) -> (crate::app_state::MeasuredDuration, MediaControlAction) {
    let action = match announced {
        Some(meta) if crate::ipc::player::same_track(measured.as_deref(), meta.id.as_deref()) => {
            MediaControlAction::SetMetadata {
                title: meta.title.clone(),
                artist: meta.artist.clone(),
                duration: Some(duration),
            }
        }
        _ => MediaControlAction::None,
    };
    (
        crate::app_state::MeasuredDuration::new(measured, duration),
        action,
    )
}

fn run_post_lock_effects(mut effects: PostLockEffects) {
    match effects.mc_action {
        MediaControlAction::SetPlayback(st) => {
            if let Some(ref mut mc) = effects.mc {
                mc.set_playback(st);
            }
        }
        MediaControlAction::SetMetadata {
            ref title,
            ref artist,
            duration,
        } => {
            if let Some(ref mut mc) = effects.mc {
                mc.set_metadata(title, artist, duration);
            }
        }
        MediaControlAction::None => {}
    }

    #[cfg(target_os = "windows")]
    if let Some(playing) = effects.thumbbar_playing
        && let Some(ref tb) = effects.thumbbar
    {
        tb.set_playing(playing);
    }

    // Put back media_controls and thumbbar
    with_state(|state| {
        if effects.mc.is_some() {
            state.media_controls = effects.mc.take();
        }
        #[cfg(target_os = "windows")]
        if effects.thumbbar.is_some() {
            state.thumbbar = effects.thumbbar.take();
        }
    });

    if let Some(batch) = effects.batch {
        run_flush_batch(batch);
    }
    if effects.should_schedule {
        schedule_flush_task();
    }
}

pub(crate) fn handle_player_event(event: PlayerEvent) {
    crate::connect::bridge::forward(&event);

    let effects = with_state(|state| {
        let mut should_flush = true;
        let mut mc_action = MediaControlAction::None;
        #[cfg(target_os = "windows")]
        let mut thumbbar_playing: Option<bool> = None;

        match event {
            PlayerEvent::TimeUpdate(time, seq) => {
                state.pending_time_update = Some((time, seq));
                if time != 0.0 {
                    should_flush = false;
                }
            }
            // Everything the Completed arm below does, minus the auto-load: the
            // incoming track is already playing, having been faded in over the
            // outgoing one. Starting it again here would restart it from zero.
            PlayerEvent::CrossfadePromoted(seq, ref track_id) => {
                crate::vprintln!("[BRIDGE] CrossfadePromoted seq={seq} track={track_id:?}");
                // The renderer needs `completed`: it is what makes TIDAL's SDK enter
                // its automatic-transition handler, which then awaits the duration
                // and the active state that follow this event.
                //
                // The OS media controls are deliberately NOT told anything. Nothing
                // stopped (the incoming track has been audible for seconds), and
                // reporting Stopped here made the taskbar and the lock screen blink
                // through a stop the listener never experienced. The `Active` that
                // follows sets them, correctly, to Playing.
                state.pending_player_events.push(PlayerBridgeEvent::state(
                    PlaybackState::Completed.as_str(),
                    seq,
                ));
            }
            PlayerEvent::StateChange(st, seq) => {
                crate::vprintln!("[BRIDGE] StateChange: \"{}\" seq={}", st.as_str(), seq);

                if st == PlaybackState::Completed {
                    let player = state.player.clone();
                    crate::state::rt_handle().spawn(async move {
                        if let Some(next) = crate::audio::preload::take_next_track().await {
                            crate::vprintln!("[AUTO]   Loading preloaded next track");
                            // The id the preload carried. This branch is the advance for
                            // SDK-native tracks, which is every FLAC: nothing re-tags one
                            // afterwards, and untagged its measured length was neither
                            // published nor kept; the track ran its whole life with no
                            // duration in the OS controls. Self-load streams (DASH, and
                            // non-FLAC BTS) advance in the renderer and tag their own load.
                            // This advance is the renderer's own queue moving. It names
                            // itself as such: the record it just took was staged by that same
                            // queue, and taking it already proved the queue is the one driving.
                            if let Err(e) = player.load_and_play(
                                next.url,
                                next.format,
                                next.key,
                                next.product_id,
                                crate::player::LoadOrigin::Local,
                            ) {
                                crate::vprintln!("[AUTO]   Failed to load next track: {e}");
                            }
                        } else {
                            crate::vprintln!("[AUTO]   No preloaded next track");
                        }
                    });
                }

                mc_action = MediaControlAction::SetPlayback(st);

                #[cfg(target_os = "windows")]
                {
                    thumbbar_playing = Some(matches!(
                        st,
                        PlaybackState::Active | PlaybackState::Seeking | PlaybackState::Idle
                    ));
                }

                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::state(st.as_str(), seq));
            }
            PlayerEvent::Duration(duration, seq, track_id) => {
                // Taken by value to keep the decision below out of the lock, and to record the
                // measurement even under a poisoned one: what the slot says only ever decided
                // whether to publish, never what the length was measured on.
                let announced = match crate::state::CURRENT_METADATA.lock() {
                    Ok(lock) => lock.clone(),
                    Err(e) => {
                        crate::vprintln!("[BRIDGE] CURRENT_METADATA lock poisoned: {e}");
                        None
                    }
                };
                let (measured, action) =
                    settle_measured_duration(duration, track_id, announced.as_ref());
                state.record_measured_duration(measured);
                mc_action = action;

                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::duration(duration, seq));
            }
            PlayerEvent::AudioDevices(devices, req_id) => {
                if let Ok(json_devices) = serde_json::to_string(&devices) {
                    if let Some(id) = req_id {
                        state
                            .pending_misc_js
                            .push(js_ipc_response(&id, &json_devices));
                    } else {
                        state
                            .pending_player_events
                            .push(PlayerBridgeEvent::devices(serde_json::json!(devices)));
                    }
                }
            }
            PlayerEvent::MediaFormat {
                codec,
                sample_rate,
                output_sample_rate,
                bit_depth,
                channels,
                bytes,
            } => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::media_format(
                        codec,
                        sample_rate,
                        output_sample_rate,
                        bit_depth,
                        channels,
                        bytes,
                    ));
                {
                    let format_json = serde_json::json!({
                        "codec": codec,
                        "sampleRate": sample_rate,
                        "outputSampleRate": output_sample_rate,
                        "bitDepth": bit_depth,
                        "channels": channels,
                        "bytes": bytes,
                    });
                    state.pending_misc_js.push(format!(
                        "(function(){{var f={};globalThis.__LUNAR_MEDIA_FORMAT__=f;var r=globalThis.__LUNAR_MEDIA_FORMAT_RESOLVERS__||[];globalThis.__LUNAR_MEDIA_FORMAT_RESOLVERS__=[];for(var i=0;i<r.length;i++)r[i](f)}})()",
                        format_json
                    ));
                }
            }
            PlayerEvent::Version(v) => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::version(v));
            }
            PlayerEvent::DeviceError(kind) => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::device_error(kind.as_str()));
            }
            PlayerEvent::MediaError { error, code } => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::media_error(&error, code.as_str()));
            }
            PlayerEvent::MaxConnectionsReached => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::max_connections());
            }
            PlayerEvent::NetworkLost => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::network_lost());
            }
            PlayerEvent::VolumeSync(v) => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::volume(v));
            }
            PlayerEvent::ReplayRequest {
                track,
                expected_gen,
                position,
                play,
            } => {
                // Reload the captured source at `position`, playing per `play`.
                // Re-check the generation: skip if a newer load/stop minted one
                // since, else re-arming would abort that newer load.
                let player = state.player.clone();
                crate::state::rt_handle().spawn(async move {
                    if crate::player::current_gen() != expected_gen {
                        crate::vprintln!(
                            "[REPLAY] re-arm skipped: superseded by a newer load/stop"
                        );
                        return;
                    }
                    crate::vprintln!("[REPLAY] re-arming retained source");
                    if let Err(e) = player.rearm(
                        track.url,
                        track.format,
                        track.key,
                        track.product_id,
                        position,
                        play,
                    ) {
                        crate::vprintln!("[REPLAY] re-arm failed: {e}");
                    }
                });
            }
        }

        let batch = if should_flush {
            Some(take_flush_batch(state))
        } else {
            None
        };

        let should_schedule = if !should_flush && !state.flush_scheduled {
            state.flush_scheduled = true;
            true
        } else {
            false
        };

        // Take media_controls/thumbbar only if needed
        let mc = if matches!(mc_action, MediaControlAction::None) {
            None
        } else {
            state.media_controls.take()
        };

        #[cfg(target_os = "windows")]
        let thumbbar = if thumbbar_playing.is_some() {
            state.thumbbar.take()
        } else {
            None
        };

        PostLockEffects {
            batch,
            should_schedule,
            mc,
            mc_action,
            #[cfg(target_os = "windows")]
            thumbbar,
            #[cfg(target_os = "windows")]
            thumbbar_playing,
        }
    });

    if let Some(effects) = effects {
        run_post_lock_effects(effects);
    }
}

fn schedule_flush_task() {
    let mut task = FlushTask::new(0);
    post_delayed_task(ThreadId::UI, Some(&mut task), 24);
}

wrap_task! {
    struct FlushTask {
        _p: u8,
    }
    impl Task {
        fn execute(&self) {
            let batch = with_state(|state| {
                state.flush_scheduled = false;
                take_flush_batch(state)
            });
            if let Some(batch) = batch {
                run_flush_batch(batch);
            }
        }
    }
}

wrap_task! {
    pub(crate) struct PlayerEventTask {
        event: PlayerEvent,
    }
    impl Task {
        fn execute(&self) {
            handle_player_event(self.event.clone());
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ui/flush.rs"]
mod tests;
