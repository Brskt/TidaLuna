use tokio::sync::mpsc;

use crate::app_state::with_state;
use crate::connect::types::{MediaInfo, PlayerState};

/// Events from the local player forwarded to the PlaybackController.
/// Each event carries an `engine_gen` for stale-event filtering.
pub(crate) enum BridgeEvent {
    Prepared {
        engine_gen: u64,
    },
    StatusUpdated {
        state: PlayerState,
        engine_gen: u64,
    },
    ProgressUpdated {
        progress_ms: u64,
        duration_ms: u64,
        engine_gen: u64,
    },
    PlaybackCompleted {
        has_next_media: bool,
        engine_gen: u64,
    },
    PlaybackError {
        status_code: String,
        engine_gen: u64,
    },
}

/// Bridge between TIDAL Connect receiver and the local Rust player.
/// Translates Connect commands into Player API calls via `with_state`.
pub(crate) struct SpeakerBridge;

impl SpeakerBridge {
    pub fn new(_event_tx: mpsc::Sender<BridgeEvent>) -> Self {
        Self
    }

    /// Load and play a track. Extracts url/format/key from MediaInfo.
    /// Supports both direct streams (BTS/FLAC) and DASH (AAC).
    pub fn prepare(&self, media_info: &MediaInfo) {
        let url = match &media_info.src_url {
            Some(url) => url.clone(),
            None => {
                crate::vprintln!("[connect::bridge] MediaInfo without srcUrl - cannot load");
                return;
            }
        };

        let format = media_info
            .stream_type
            .clone()
            .unwrap_or_else(|| "progressive".to_string());

        // DASH path: init_url in src_url, segment URLs in custom_data
        let dash_segments = media_info
            .custom_data
            .as_ref()
            .and_then(|cd| cd.get("dashSegmentUrls"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            });

        if let Some(segments) = dash_segments {
            crate::vprintln!(
                "[connect::bridge] Loading DASH: {} segments, codec={}",
                segments.len(),
                format
            );
            with_state(|state| {
                if let Err(e) = state.player.load_dash(url, segments, format) {
                    crate::vprintln!("[connect::bridge] Player DASH load error: {}", e);
                }
            });
            return;
        }

        // Direct stream path (BTS/FLAC)
        let key = media_info
            .custom_data
            .as_ref()
            .and_then(|cd| cd.get("encryptionKey"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        with_state(|state| {
            if let Err(e) = state.player.load_and_play(url, format, key) {
                crate::vprintln!("[connect::bridge] Player load error: {}", e);
            }
        });
    }

    /// No-op - gapless is handled by PlaybackController calling set_media() on completion.
    pub fn prepare_next(&self, _media_info: &MediaInfo) {
        crate::vprintln!("[connect::bridge] prepare_next - gapless via set_media on completion");
    }

    pub fn play(&self) {
        with_state(|state| {
            let _ = state.player.play();
        });
    }

    pub fn pause(&self) {
        with_state(|state| {
            let _ = state.player.pause();
        });
    }

    pub fn stop(&self) {
        with_state(|state| {
            let _ = state.player.stop();
        });
    }

    /// Seek to position in milliseconds.
    pub fn seek(&self, position_ms: u64) {
        let seconds = position_ms as f64 / 1000.0;
        with_state(|state| {
            let _ = state.player.seek(seconds);
        });
    }

    /// Set volume. Connect uses 0.0-1.0, Player uses 0-100.
    pub fn set_volume(&self, level: f64) {
        let volume = (level * 100.0).clamp(0.0, 100.0);
        with_state(|state| {
            let _ = state.player.set_volume(volume);
        });
    }

    /// Mute by setting volume to 0 (Player has no native mute).
    pub fn set_mute(&self, mute: bool, saved_level: f64) {
        let volume = if mute {
            0.0
        } else {
            (saved_level * 100.0).clamp(0.0, 100.0)
        };
        with_state(|state| {
            let _ = state.player.set_volume(volume);
        });
    }
}
