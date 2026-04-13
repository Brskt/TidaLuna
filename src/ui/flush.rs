use crate::app_state::{AppState, exec_js_on_frame, with_state};
use crate::bridge::PlayerBridgeEvent;
use crate::connect::receiver::speaker_bridge::BridgeEvent;
use crate::connect::types::PlayerState as ConnectPlayerState;
use crate::player::{PlaybackState, PlayerEvent};
use cef::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::mpsc;

/// Monotonic counter stamped on each bridge event. The receiver drops events
/// whose generation doesn't match the current media, preventing stale events
/// from a previous track from affecting the new one after rapid skips.
pub(crate) static CONNECT_ENGINE_GEN: AtomicU64 = AtomicU64::new(0);

/// Fast check to skip the mutex lock when no receiver is active.
static CONNECT_BRIDGE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Sender for Connect bridge events. Set when receiver is active, None otherwise.
static CONNECT_BRIDGE_TX: std::sync::Mutex<Option<mpsc::Sender<BridgeEvent>>> =
    std::sync::Mutex::new(None);

pub(crate) fn set_connect_bridge_tx(tx: Option<mpsc::Sender<BridgeEvent>>) {
    let active = tx.is_some();
    *CONNECT_BRIDGE_TX.lock().unwrap() = tx;
    CONNECT_BRIDGE_ACTIVE.store(active, Ordering::Release);
}

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
    forward_to_connect_bridge(&event);

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
            PlayerEvent::StateChange(st, seq) => {
                crate::vprintln!("[BRIDGE] StateChange: \"{}\" seq={}", st.as_str(), seq);

                if st == PlaybackState::Completed {
                    let player = state.player.clone();
                    crate::state::rt_handle().spawn(async move {
                        if let Some(next) = crate::audio::preload::take_next_track().await {
                            crate::vprintln!("[AUTO]   Loading preloaded next track");
                            if let Err(e) = player.load_and_play(next.url, next.format, next.key) {
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
            PlayerEvent::Duration(duration, seq) => {
                state.media_duration = Some(duration);

                match crate::state::CURRENT_METADATA.lock() {
                    Ok(lock) => {
                        if let Some(ref meta) = *lock {
                            mc_action = MediaControlAction::SetMetadata {
                                title: meta.title.clone(),
                                artist: meta.artist.clone(),
                                duration: Some(duration),
                            };
                        }
                    }
                    Err(e) => crate::vprintln!("[BRIDGE] CURRENT_METADATA lock poisoned: {e}"),
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
                    .push(PlayerBridgeEvent::media_error(&error, code));
            }
            PlayerEvent::MaxConnectionsReached => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::max_connections());
            }
            PlayerEvent::VolumeSync(v) => {
                state
                    .pending_player_events
                    .push(PlayerBridgeEvent::volume(v));
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

/// Forward player events to the Connect receiver bridge (if active).
fn forward_to_connect_bridge(event: &PlayerEvent) {
    if !CONNECT_BRIDGE_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    if matches!(event, PlayerEvent::TimeUpdate(..)) {
        // Log once to confirm the bridge forwarding path is live
        static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::Relaxed) {
            crate::vprintln!("[connect::bridge] First TimeUpdate forwarded to receiver");
        }
    }
    let guard = match CONNECT_BRIDGE_TX.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let tx = match guard.as_ref() {
        Some(tx) => tx,
        None => return,
    };

    let engine_gen = CONNECT_ENGINE_GEN.load(Ordering::Relaxed);

    let bridge_event = match event {
        PlayerEvent::StateChange(state, _seq) => match state {
            PlaybackState::Ready => Some(BridgeEvent::Prepared { engine_gen }),
            PlaybackState::Active => Some(BridgeEvent::StatusUpdated {
                state: ConnectPlayerState::Playing,
                engine_gen,
            }),
            PlaybackState::Paused => Some(BridgeEvent::StatusUpdated {
                state: ConnectPlayerState::Paused,
                engine_gen,
            }),
            PlaybackState::Idle => Some(BridgeEvent::StatusUpdated {
                state: ConnectPlayerState::Buffering,
                engine_gen,
            }),
            PlaybackState::Completed => Some(BridgeEvent::PlaybackCompleted {
                has_next_media: false,
                engine_gen,
            }),
            PlaybackState::Stopped => Some(BridgeEvent::StatusUpdated {
                state: ConnectPlayerState::Idle,
                engine_gen,
            }),
            _ => None,
        },
        PlayerEvent::TimeUpdate(seconds, _seq) => {
            let ms = (*seconds * 1000.0) as u64;
            Some(BridgeEvent::ProgressUpdated {
                progress_ms: ms,
                duration_ms: 0,
                engine_gen,
            })
        }
        PlayerEvent::MediaError { error, .. } => Some(BridgeEvent::PlaybackError {
            status_code: error.clone(),
            engine_gen,
        }),
        _ => None,
    };

    if let Some(evt) = bridge_event {
        let _ = tx.try_send(evt);
    }
}
