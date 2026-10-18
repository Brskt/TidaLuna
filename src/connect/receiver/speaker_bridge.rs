use crate::app_state::with_state;
use crate::connect::types::{MediaInfo, PlayerState};

/// Which load a player event comes from, as the bridge hands it over.
///
/// `engine_gen` is the controller's generation, read when the event is forwarded rather than
/// when it was emitted: a local track's end still queued when the cast's first load starts
/// reads that load's generation. `seq` is the event generation the player stamped on the event
/// as it emitted it, and only it names the load that did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EventStamp {
    pub(crate) engine_gen: u64,
    pub(crate) seq: u32,
}

impl EventStamp {
    /// Whether the event comes from the load the controller handed the player at `engine_gen`,
    /// whose events start at `load_seq`. A later seq is that load's too, or a command the player
    /// took over it since (a stop, a re-arm); an earlier one belongs to a load before it.
    pub(crate) fn is_from(self, engine_gen: u64, load_seq: u32) -> bool {
        self.engine_gen == engine_gen && self.seq >= load_seq
    }
}

/// Events from this host forwarded to the receiver: the local player's, each stamped with the
/// load it comes from, and the loads the SDK asks for during a cast.
pub(crate) enum BridgeEvent {
    Prepared {
        from: EventStamp,
    },
    StatusUpdated {
        state: PlayerState,
        from: EventStamp,
    },
    ProgressUpdated {
        progress_ms: u64,
        duration_ms: u64,
        from: EventStamp,
    },
    PlaybackCompleted {
        has_next_media: bool,
        from: EventStamp,
    },
    PlaybackError {
        status_code: String,
        from: EventStamp,
    },
    /// The local decoder measured the track's real length. The queue metadata is the only other
    /// source the controller ever sees, and window items reach us without one. Carries the track
    /// it was measured on: a measurement outliving its track must not be charged to the next.
    DurationMeasured {
        duration_ms: u64,
        track_id: Option<String>,
        from: EventStamp,
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
    /// The event generation the load's events start at, `None` when the track never reached
    /// the player. No `PlayerEvent` follows a refusal; the caller's `Preparing` would stand
    /// forever unless it answers for the failure itself.
    pub fn prepare(&self, media_info: &MediaInfo) -> Option<u32> {
        let url = match &media_info.src_url {
            Some(url) => url.clone(),
            None => {
                crate::verr!("[connect::bridge] MediaInfo without srcUrl - cannot load");
                return None;
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
                    Ok(seq) => Some(seq),
                    Err(e) => {
                        crate::verr!("[connect::bridge] Player DASH load error: {}", e);
                        None
                    }
                }
            })
            .flatten();
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
            state.player.load_and_play(
                url,
                format,
                key,
                media_id,
                crate::player::LoadOrigin::Connect,
            )
        })
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
