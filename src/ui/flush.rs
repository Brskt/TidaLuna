use crate::app_state::{AppState, exec_js_on_frame, with_state};
use crate::bridge::PlayerBridgeEvent;
use crate::player::{PlaybackState, PlayerEvent};
use cef::*;

pub(crate) fn flush_bridge_now(state: &mut AppState) {
    if let Some((time, seq)) = state.pending_time_update.take() {
        state
            .pending_player_events
            .push(PlayerBridgeEvent::time(time, seq));
    }

    if !state.pending_player_events.is_empty() {
        if let Ok(events_json) = serde_json::to_string(&state.pending_player_events) {
            let js = format!(
                "if (window.__TIDAL_RS_PLAYER_PUSH__) {{ window.__TIDAL_RS_PLAYER_PUSH__({}); }}",
                events_json
            );
            if let Some(ref browser) = state.browser
                && let Some(frame) = browser.main_frame()
            {
                exec_js_on_frame(&frame, &js);
            } else {
                crate::vprintln!(
                    "[BRIDGE] flush DROPPED — {}",
                    if state.browser.is_none() {
                        "no browser"
                    } else {
                        "no frame"
                    }
                );
            }
        }
        state.pending_player_events.clear();
    }

    if !state.pending_misc_js.is_empty() {
        let js_batch = state.pending_misc_js.join(";");
        if let Some(ref browser) = state.browser
            && let Some(frame) = browser.main_frame()
        {
            exec_js_on_frame(&frame, &js_batch);
        }
        state.pending_misc_js.clear();
    }
}

pub(crate) fn handle_player_event(event: PlayerEvent) {
    with_state(|state| {
        let mut should_flush = true;
        match event {
            PlayerEvent::TimeUpdate(time, seq) => {
                state.pending_time_update = Some((time, seq));
                if time != 0.0 {
                    should_flush = false;
                }
            }
            PlayerEvent::StateChange(st, seq) => {
                crate::vprintln!("[BRIDGE] StateChange: \"{}\" seq={}", st.as_str(), seq);

                if st == PlaybackState::Completed {
                    let player = state.player.clone();
                    state.rt_handle.spawn(async move {
                        if let Some(next) = crate::audio::preload::take_next_track().await {
                            crate::vprintln!("[AUTO]   Loading preloaded next track");
                            if let Err(e) = player.load_and_play(next.url, next.format, next.key) {
                                eprintln!("[AUTO]   Failed to load next track: {e}");
                            }
                        } else {
                            crate::vprintln!("[AUTO]   No preloaded next track");
                        }
                    });
                }

                if let Some(ref mut mc) = state.media_controls {
                    mc.set_playback(st);
                }

                #[cfg(target_os = "windows")]
                if let Some(ref tb) = state.thumbbar {
                    tb.set_playing(matches!(
                        st,
                        PlaybackState::Active | PlaybackState::Seeking | PlaybackState::Idle
                    ));
                }

                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::state(st.as_str(), seq));
            }
            PlayerEvent::Duration(duration, seq) => {
                state.media_duration = Some(duration);

                if let Some(ref mut mc) = state.media_controls
                    && let Some(ref meta) = *crate::state::CURRENT_METADATA
                        .lock()
                        .expect("CURRENT_METADATA lock poisoned")
                {
                    mc.set_metadata(&meta.title, &meta.artist, Some(duration));
                }

                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::duration(duration, seq));
            }
            PlayerEvent::AudioDevices(devices, req_id) => {
                if let Ok(json_devices) = serde_json::to_string(&devices) {
                    if let Some(id) = req_id {
                        let js = format!(
                            "window.__TIDAL_IPC_RESPONSE__('{}', null, {})",
                            id, json_devices
                        );
                        state.pending_misc_js.push(js);
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
                bit_depth,
                channels,
                bytes,
            } => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::media_format(
                        codec,
                        sample_rate,
                        bit_depth,
                        channels,
                        bytes,
                    ));
                {
                    let format_json = serde_json::json!({
                        "codec": codec,
                        "sampleRate": sample_rate,
                        "bitDepth": bit_depth,
                        "channels": channels,
                        "bytes": bytes,
                    });
                    state.pending_misc_js.push(format!(
                        "(function(){{var f={};globalThis.__LUNAR_MEDIA_FORMAT__=f;var r=globalThis.__LUNAR_MEDIA_FORMAT_RESOLVERS__;globalThis.__LUNAR_MEDIA_FORMAT_RESOLVERS__=[];for(var i=0;i<r.length;i++)r[i](f)}})()",
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
                    .push(PlayerBridgeEvent::media_error(&error, code));
            }
            PlayerEvent::MaxConnectionsReached => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::max_connections());
            }
        }
        if should_flush {
            flush_bridge_now(state);
        } else if !state.flush_scheduled {
            state.flush_scheduled = true;
            schedule_flush_task();
        }
    });
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
            with_state(|state| {
                state.flush_scheduled = false;
                flush_bridge_now(state);
            });
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
