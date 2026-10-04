use crate::app_state::with_state;
use crate::connect::types::{MediaInfo, PlayerState};

/// Events from this host forwarded to the receiver: the local player's, each carrying an
/// `engine_gen` for stale-event filtering, and the loads the SDK asks for during a cast.
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
    /// A local crossfade finished and the next track is already playing, seconds in. It reads
    /// like a completion and is the opposite of one: a completion asks the controller to
    /// prepare what comes next, where this one reports something it must not prepare again.
    /// Routed as a completion, the reload it triggers is audible: the track that just faded
    /// in stops and restarts from zero.
    ///
    /// Carries the track promoted, for the same reason `DurationMeasured` carries one: without
    /// it the receiver has to infer what moved from its own queue index, and the length the
    /// player measures in the same breath has nothing to be judged against.
    CrossfadeTransitioned {
        track_id: Option<String>,
        engine_gen: u64,
    },
    PlaybackError {
        status_code: String,
        engine_gen: u64,
    },
    /// The local decoder measured the track's real length. The queue metadata is the only other
    /// source the controller ever sees, and window items reach us without one. Carries the track
    /// it was measured on: a measurement outliving its track must not be charged to the next.
    DurationMeasured {
        duration_ms: u64,
        track_id: Option<String>,
        engine_gen: u64,
    },
    /// The SDK on this host asked for a load while the controller owns the player. The loop
    /// decides what it becomes against the controller's queue, and answers it whatever it
    /// decides: the SDK waits on that load with no timeout.
    LocalLoad(crate::player::ipc::CastLoad),
}

/// Bridge between TIDAL Connect receiver and the local Rust player.
/// Translates Connect commands into Player API calls via `with_state`.
pub(crate) struct SpeakerBridge;

impl Default for SpeakerBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeakerBridge {
    pub fn new() -> Self {
        Self
    }

    /// Load and play a track. Extracts url/format/key from MediaInfo.
    /// Supports both direct streams (BTS/FLAC) and DASH (AAC).
    ///
    /// `false` when the track never reached the player. No `PlayerEvent` follows a refusal, so
    /// the caller's `Preparing` would stand forever unless it answers for the failure itself.
    pub fn prepare(&self, media_info: &MediaInfo) -> bool {
        let url = match &media_info.src_url {
            Some(url) => url.clone(),
            None => {
                crate::verr!("[connect::bridge] MediaInfo without srcUrl - cannot load");
                return false;
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
            // Named in Connect's space from end to end, never compared against the
            // frontend's: the receiver's metadata task records this same id.
            let media_id = media_info.track_id();
            return with_state(|state| {
                match state.player.load_dash(
                    url,
                    segments,
                    format,
                    media_id,
                    crate::player::LoadOrigin::Connect,
                ) {
                    Ok(()) => true,
                    Err(e) => {
                        crate::verr!("[connect::bridge] Player DASH load error: {}", e);
                        false
                    }
                }
            })
            .unwrap_or(false);
        }

        // Direct stream path (BTS/FLAC)
        let key = media_info
            .custom_data
            .as_ref()
            .and_then(|cd| cd.get("encryptionKey"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let media_id = media_info.track_id();
        with_state(|state| {
            match state.player.load_and_play(
                url,
                format,
                key,
                media_id,
                crate::player::LoadOrigin::Connect,
            ) {
                Ok(()) => true,
                Err(e) => {
                    crate::verr!("[connect::bridge] Player load error: {}", e);
                    false
                }
            }
        })
        .unwrap_or(false)
    }

    /// The player thread is the only receiver and nothing restarts it: a refused send means
    /// this command and every later one is gone. The controller has no reply channel here, which
    /// is exactly why the log has to be ungated.
    fn send_transport(command: &str, sent: anyhow::Result<()>) {
        if let Err(e) = sent {
            crate::verr!("[connect::bridge] {command} not delivered: {e}");
        }
    }

    /// No-op - gapless is handled by PlaybackController calling set_media() on completion.
    pub fn prepare_next(&self, _media_info: &MediaInfo) {
        crate::vprintln!("[connect::bridge] prepare_next - gapless via set_media on completion");
    }

    pub fn play(&self) {
        with_state(|state| Self::send_transport("play", state.player.play()));
    }

    pub fn pause(&self) {
        with_state(|state| Self::send_transport("pause", state.player.pause()));
    }

    pub fn stop(&self) {
        with_state(|state| {
            Self::send_transport(
                "stop",
                state.player.stop(crate::player::StopScope::As(
                    crate::player::LoadOrigin::Connect,
                )),
            )
        });
    }

    /// Stop the cast, handing the player back to this host: the session that owned it is over.
    /// A stop in Connect's name would leave the controller owning a player it no longer drives,
    /// and every load the renderer asks for after it would be refused. A session that never took
    /// the player stops nothing: the music playing is this host's own.
    pub fn release(&self) {
        with_state(|state| {
            Self::send_transport("release", state.player.stop(crate::player::StopScope::Cast))
        });
    }

    /// Seek to position in milliseconds.
    pub fn seek(&self, position_ms: u64) {
        let seconds = position_ms as f64 / 1000.0;
        with_state(|state| Self::send_transport("seek", state.player.seek(seconds)));
    }

    /// Connect uses 0.0-1.0, Player uses 0-100. The level the output now plays at, `None` when
    /// the command never reached the player.
    pub fn set_volume(&self, level: f64) -> Option<crate::player::OutputLevel> {
        let volume = (level * 100.0).clamp(0.0, 100.0);
        with_state(|state| match state.player.set_remote_volume(volume) {
            Ok(level) => Some(level),
            Err(e) => {
                Self::send_transport("setVolume", Err(e));
                None
            }
        })
        .flatten()
    }

    /// The level the output now plays at, `None` when the command never reached the player.
    pub fn set_mute(&self, mute: bool) -> Option<crate::player::OutputLevel> {
        with_state(|state| match state.player.set_mute(mute) {
            Ok(level) => Some(level),
            Err(e) => {
                Self::send_transport("setMute", Err(e));
                None
            }
        })
        .flatten()
    }
}
