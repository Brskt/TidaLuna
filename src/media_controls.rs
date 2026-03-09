use std::time::Duration;

use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, PlatformConfig};

use crate::player::PlaybackState;

pub(crate) struct OsMediaControls {
    controls: MediaControls,
}

impl OsMediaControls {
    pub fn new(hwnd: Option<*mut std::ffi::c_void>) -> Option<Self> {
        crate::vprintln!("[MEDIA]  Initializing OS media controls...");
        crate::vprintln!("[MEDIA]  HWND = {hwnd:?}");

        let config = PlatformConfig {
            dbus_name: "tidalunar",
            display_name: "TidaLunar",
            hwnd,
        };

        let mut controls = match MediaControls::new(config) {
            Ok(c) => {
                crate::vprintln!("[MEDIA]  MediaControls::new() OK");
                c
            }
            Err(e) => {
                crate::vprintln!("[MEDIA]  MediaControls::new() FAILED: {e}");
                return None;
            }
        };

        match controls.attach(handle_media_event) {
            Ok(()) => crate::vprintln!("[MEDIA]  attach() OK — buttons enabled, type=Music"),
            Err(e) => {
                crate::vprintln!("[MEDIA]  attach() FAILED: {e}");
                return None;
            }
        }

        match controls.set_metadata(MediaMetadata {
            title: Some("TidaLunar"),
            artist: Some(""),
            album: None,
            cover_url: None,
            duration: None,
        }) {
            Ok(()) => crate::vprintln!("[MEDIA]  Initial metadata set OK (title=\"TidaLunar\")"),
            Err(e) => crate::vprintln!("[MEDIA]  Initial metadata FAILED: {e}"),
        }

        match controls.set_playback(MediaPlayback::Paused { progress: None }) {
            Ok(()) => crate::vprintln!("[MEDIA]  Initial playback state set OK (Paused)"),
            Err(e) => crate::vprintln!("[MEDIA]  Initial playback state FAILED: {e}"),
        }

        crate::vprintln!("[MEDIA]  Initialization complete");
        Some(Self { controls })
    }

    pub fn set_playback(&mut self, state: PlaybackState) {
        let (playback, label) = match state {
            PlaybackState::Active | PlaybackState::Seeking | PlaybackState::Idle => {
                (MediaPlayback::Playing { progress: None }, "Playing")
            }
            PlaybackState::Paused | PlaybackState::Ready => {
                (MediaPlayback::Paused { progress: None }, "Paused")
            }
            _ => (MediaPlayback::Stopped, "Stopped"),
        };
        crate::vprintln!("[MEDIA]  set_playback: {} → SMTC {}", state.as_str(), label);
        match self.controls.set_playback(playback) {
            Ok(()) => crate::vprintln!("[MEDIA]  set_playback OK"),
            Err(e) => crate::vprintln!("[MEDIA]  set_playback FAILED: {e}"),
        }
    }

    pub fn set_metadata(&mut self, title: &str, artist: &str, duration_secs: Option<f64>) {
        let duration = duration_secs
            .filter(|d| d.is_finite() && *d > 0.0)
            .map(Duration::from_secs_f64);

        crate::vprintln!(
            "[MEDIA]  set_metadata: title=\"{title}\", artist=\"{artist}\", duration={duration_secs:?}"
        );

        let metadata = MediaMetadata {
            title: Some(title),
            artist: Some(artist),
            album: None,
            cover_url: None,
            duration,
        };
        match self.controls.set_metadata(metadata) {
            Ok(()) => crate::vprintln!("[MEDIA]  set_metadata OK"),
            Err(e) => crate::vprintln!("[MEDIA]  set_metadata FAILED: {e}"),
        }
    }
}

fn handle_media_event(event: MediaControlEvent) {
    match event {
        MediaControlEvent::Play => {
            crate::vprintln!("[MEDIA]  Play");
            crate::eval_js("window.__TL_PLAY_PAUSE__?.()");
        }
        MediaControlEvent::Pause => {
            crate::vprintln!("[MEDIA]  Pause");
            crate::eval_js("window.__TL_PLAY_PAUSE__?.()");
        }
        MediaControlEvent::Toggle => {
            crate::vprintln!("[MEDIA]  Toggle");
            crate::eval_js("window.__TL_PLAY_PAUSE__?.()");
        }
        MediaControlEvent::Next => {
            crate::vprintln!("[MEDIA]  Next");
            crate::eval_js("window.__TIDAL_PLAYBACK_DELEGATE__?.playNext?.();");
        }
        MediaControlEvent::Previous => {
            crate::vprintln!("[MEDIA]  Previous");
            crate::eval_js("window.__TIDAL_PLAYBACK_DELEGATE__?.playPrevious?.();");
        }
        MediaControlEvent::Stop => {
            crate::vprintln!("[MEDIA]  Stop");
            crate::with_state(|state| {
                let _ = state.player.stop();
            });
        }
        _ => {}
    }
}
