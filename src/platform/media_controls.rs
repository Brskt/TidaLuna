use cef::*;
use std::time::Duration;

use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, PlatformConfig};

use crate::player::PlaybackState;

pub(crate) struct OsMediaControls {
    controls: MediaControls,
    /// What the OS was last told by `set_metadata`, held so an unchanged announcement can be
    /// skipped. `None` until the first one lands, and only ever written after a successful call:
    /// a failed one leaves the OS holding something else, which the next call must correct.
    announced: Option<(String, String, Option<Duration>)>,
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
            Ok(()) => crate::vprintln!("[MEDIA]  attach() OK - buttons enabled, type=Music"),
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
        // The placeholder above is deliberately not recorded: it is not a track, and leaving the
        // memo empty means the first real announcement always reaches the OS.
        Some(Self {
            controls,
            announced: None,
        })
    }

    pub fn set_playback(&mut self, state: PlaybackState) {
        let (playback, label) = match state {
            PlaybackState::Active | PlaybackState::Seeking | PlaybackState::Idle => {
                (MediaPlayback::Playing { progress: None }, "Playing")
            }
            PlaybackState::Paused | PlaybackState::Ready => {
                (MediaPlayback::Paused { progress: None }, "Paused")
            }
            PlaybackState::Stopped | PlaybackState::Completed => {
                (MediaPlayback::Stopped, "Stopped")
            }
        };
        crate::vprintln!(
            "[MEDIA]  set_playback: {} -> SMTC {}",
            state.as_str(),
            label
        );
        match self.controls.set_playback(playback) {
            Ok(()) => crate::vprintln!("[MEDIA]  set_playback OK"),
            Err(e) => crate::vprintln!("[MEDIA]  set_playback FAILED: {e}"),
        }
    }

    pub fn set_metadata(&mut self, title: &str, artist: &str, duration_secs: Option<f64>) {
        let duration = duration_secs
            .filter(|d| d.is_finite() && *d > 0.0)
            .map(Duration::from_secs_f64);

        // Skipped when unchanged: the page re-announces a track on every reference change, and the
        // flush's measured-duration settle and the Connect receiver announce on their own. Guarded
        // here, the one place that knows what the OS holds, because souvlaki's Windows backend is
        // synchronous WinRT/COM on the CEF UI thread. The normalised duration is compared: two
        // lengths the filter above drops are one announcement.
        let unchanged =
            self.announced
                .as_ref()
                .is_some_and(|(had_title, had_artist, had_duration)| {
                    had_title == title && had_artist == artist && *had_duration == duration
                });
        if unchanged {
            crate::vprintln3!("[MEDIA]  set_metadata: unchanged, skipped");
            return;
        }

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
            Ok(()) => {
                self.announced = Some((title.to_owned(), artist.to_owned(), duration));
                crate::vprintln!("[MEDIA]  set_metadata OK");
            }
            Err(e) => crate::vprintln!("[MEDIA]  set_metadata FAILED: {e}"),
        }
    }
}

/// The subset of media-key actions the app reacts to. Resolved from a
/// `MediaControlEvent` on souvlaki's callback thread, then carried to the CEF
/// UI thread where the work is actually performed.
#[derive(Clone, Copy)]
enum MediaAction {
    PlayPause,
    Next,
    Prev,
    Stop,
}

fn handle_media_event(event: MediaControlEvent) {
    // souvlaki invokes this on its own SMTC (Windows) / MPRIS (Linux) thread.
    // `eval_js`/`with_state` clone and dereference the CEF `Browser`/`Frame`
    // handles, which is only sound on the CEF UI thread - the exact invariant
    // `unsafe impl Send for AppState` depends on. Resolve the action here, then
    // marshal it to the UI thread before touching any of those handles.
    let action = match event {
        MediaControlEvent::Play | MediaControlEvent::Pause | MediaControlEvent::Toggle => {
            MediaAction::PlayPause
        }
        MediaControlEvent::Next => MediaAction::Next,
        MediaControlEvent::Previous => MediaAction::Prev,
        MediaControlEvent::Stop => MediaAction::Stop,
        _ => return,
    };
    let mut task = MediaEventTask::new(action);
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct MediaEventTask {
        action: MediaAction,
    }
    impl Task {
        fn execute(&self) {
            dispatch_media_event(self.action);
        }
    }
}

fn dispatch_media_event(action: MediaAction) {
    match action {
        MediaAction::PlayPause => {
            crate::vprintln!("[MEDIA]  Play/Pause");
            crate::app_state::eval_js(super::js_actions::PLAY_PAUSE);
        }
        MediaAction::Next => {
            crate::vprintln!("[MEDIA]  Next");
            crate::app_state::eval_js(super::js_actions::PLAY_NEXT);
        }
        MediaAction::Prev => {
            crate::vprintln!("[MEDIA]  Previous");
            crate::app_state::eval_js(super::js_actions::PLAY_PREV);
        }
        MediaAction::Stop => {
            crate::vprintln!("[MEDIA]  Stop");
            crate::app_state::with_state(|state| {
                let _ = state.player.stop(crate::player::LoadOrigin::Local);
            });
        }
    }
}
