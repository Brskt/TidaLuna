use tokio::sync::mpsc;

use crate::connect::receiver::speaker_bridge::SpeakerBridge;
use crate::connect::types::{
    MediaInfo, MediaMetadata, PbPlayState, PbState, PlaybackNotification, PlayerState,
};

// ── Events emitted to clients and other services ─────────────────────

pub(crate) enum PlaybackNotifyEvent {
    Broadcast(serde_json::Value),
    Internal(PlaybackInternalEvent),
}

pub(crate) enum PlaybackInternalEvent {
    MediaCompleted { has_next: bool },
}

// ── Gapless next-media slot (with queue_gen for invalidation) ────────

struct NextMediaSlot {
    media: MediaInfo,
    media_seq_no: u32,
    queue_gen: u64,
}

// ── Promotion the queue has not caught up with ───────────────────────

/// The track a local crossfade moved to, held until the queue's `AdoptMedia` arrives.
///
/// The player names the promoted track and measures its length in the same breath, but the
/// adoption that would install it travels three hops and an HTTP round trip behind. In that
/// window `current_media` still names the outgoing track, so a measurement judged against it
/// names the wrong one and the guard refusing stale figures drops the only length this track
/// will ever get. `engine_gen` is stamped rather than cleared: a promotion belongs to the engine
/// session that made it, and a session that ends leaves nothing behind to forget.
struct PendingPromotion {
    engine_gen: u64,
    track_id: String,
    measured_duration_ms: Option<u64>,
}

// ── PlaybackController ──────────────────────────────────────────────

pub(crate) struct PlaybackController {
    state: PbState,
    play_state: PbPlayState,

    current_media: Option<MediaInfo>,
    next_media: Option<NextMediaSlot>,
    pending_promotion: Option<PendingPromotion>,
    media_seq_no: u32,

    /// Engine generation - incremented on every set_media/reset
    engine_gen: u64,
    /// Queue generation - set by QueueManager, checked on gapless swap
    queue_gen: u64,

    bridge: SpeakerBridge,
    notify_tx: mpsc::Sender<PlaybackNotifyEvent>,

    last_progress_ms: u64,
    current_duration_ms: u64,

    /// Saved volume level for mute/unmute
    saved_volume_level: f64,
    muted: bool,
    /// Last time we broadcast progress (for throttling)
    last_progress_broadcast: std::time::Instant,
}

impl PlaybackController {
    pub fn new(bridge: SpeakerBridge, notify_tx: mpsc::Sender<PlaybackNotifyEvent>) -> Self {
        Self {
            state: PbState::NoEngine,
            play_state: PbPlayState::Paused,
            current_media: None,
            next_media: None,
            pending_promotion: None,
            media_seq_no: 0,
            engine_gen: 0,
            queue_gen: 0,
            bridge,
            notify_tx,
            last_progress_ms: 0,
            current_duration_ms: 0,
            saved_volume_level: 1.0,
            muted: false,
            last_progress_broadcast: std::time::Instant::now(),
        }
    }

    // ── Commands from mobile clients ─────────────────────────────────

    /// Load a media item for playback.
    pub async fn set_media(&mut self, media_info: MediaInfo, media_seq_no: u32) {
        if media_seq_no < self.media_seq_no {
            crate::vprintln!(
                "[connect::playback] Stale media seq: now={}, recv={}",
                self.media_seq_no,
                media_seq_no
            );
            return;
        }
        self.media_seq_no = media_seq_no;
        self.engine_gen += 1;
        // Sync the shared atomic for flush.rs to stamp events with the same gen
        crate::connect::bridge::ENGINE_GEN
            .store(self.engine_gen, std::sync::atomic::Ordering::Relaxed);

        // Stop current playback if active
        if self.state == PbState::Started || self.state == PbState::Preparing {
            self.bridge.stop();
        }

        self.current_media = Some(media_info.clone());
        self.last_progress_ms = 0;
        self.current_duration_ms = media_info
            .metadata
            .as_ref()
            .and_then(|m| m.duration)
            .unwrap_or(0);
        self.state = PbState::Preparing;
        self.play_state = PbPlayState::Playing; // Default intent: autoplay

        let handed_to_player = self.bridge.prepare(&media_info);
        self.notify_media_changed().await;
        if !handed_to_player {
            // Nothing reached the player: no `PlayerEvent` will ever arrive, and `Preparing`
            // would stand for the rest of the session. Answer for it here, where the state that
            // would be stranded lives.
            self.on_playback_error("media could not be loaded", self.engine_gen)
                .await;
        }
    }

    /// Prepare next media for gapless playback.
    pub async fn set_next_media(&mut self, media_info: MediaInfo, media_seq_no: u32) {
        self.next_media = Some(NextMediaSlot {
            media: media_info.clone(),
            media_seq_no,
            queue_gen: self.queue_gen,
        });
        self.bridge.prepare_next(&media_info);
    }

    pub async fn play(&mut self) {
        match self.state {
            PbState::NoEngine | PbState::Stopped => {
                crate::vprintln!(
                    "[connect::playback] Illegal state for play: {:?}",
                    self.state
                );
            }
            PbState::Started => {
                if self.play_state != PbPlayState::Playing {
                    self.play_state = PbPlayState::Playing;
                    self.bridge.play();
                    self.notify_player_status().await;
                }
            }
            PbState::Preparing => {
                // Store intent - will be applied in on_prepared()
                self.play_state = PbPlayState::Playing;
            }
            PbState::Completed => {
                crate::vprintln!("[connect::playback] Play after completed - ignored");
            }
        }
    }

    pub async fn pause(&mut self) {
        match self.state {
            PbState::NoEngine | PbState::Stopped => {
                crate::vprintln!(
                    "[connect::playback] Illegal state for pause: {:?}",
                    self.state
                );
            }
            PbState::Started => {
                if self.play_state != PbPlayState::Paused {
                    self.play_state = PbPlayState::Paused;
                    self.bridge.pause();
                    self.notify_player_status().await;
                }
            }
            PbState::Preparing | PbState::Completed => {
                self.play_state = PbPlayState::Paused;
            }
        }
    }

    pub async fn stop(&mut self) {
        if matches!(self.state, PbState::NoEngine) {
            return;
        }
        self.state = PbState::Stopped;
        self.engine_gen += 1;
        crate::connect::bridge::ENGINE_GEN
            .store(self.engine_gen, std::sync::atomic::Ordering::Relaxed);
        self.bridge.stop();
        self.current_media = None;
        self.next_media = None;
        self.notify_player_status().await;
    }

    pub async fn seek(&mut self, position_ms: u64) {
        if matches!(self.state, PbState::NoEngine | PbState::Stopped) {
            crate::vprintln!(
                "[connect::playback] Illegal state for seek: {:?}",
                self.state
            );
            return;
        }
        self.bridge.seek(position_ms);
    }

    pub async fn set_volume(&mut self, level: f64) {
        self.saved_volume_level = level;
        if !self.muted {
            self.bridge.set_volume(level);
        }
    }

    pub async fn set_mute(&mut self, mute: bool) {
        self.muted = mute;
        self.bridge.set_mute(mute, self.saved_volume_level);
    }

    pub async fn reset(&mut self) {
        self.engine_gen += 1;
        crate::connect::bridge::ENGINE_GEN
            .store(self.engine_gen, std::sync::atomic::Ordering::Relaxed);
        self.state = PbState::NoEngine;
        self.current_media = None;
        self.next_media = None;
        self.last_progress_ms = 0;
        self.current_duration_ms = 0;
        self.bridge.stop();
    }

    /// Called by QueueManager when queue_gen changes.
    pub fn set_queue_gen(&mut self, new_gen: u64) {
        self.queue_gen = new_gen;
    }

    // ── Callbacks from the player (via BridgeEvent) ──────────────────

    /// Player finished preparing (Ready state).
    pub async fn on_prepared(&mut self, engine_gen: u64) {
        if engine_gen != self.engine_gen {
            crate::vprintln!(
                "[connect::playback] Stale prepared: gen={}, current={}",
                engine_gen,
                self.engine_gen
            );
            return;
        }
        if self.state != PbState::Preparing {
            return;
        }

        self.state = PbState::Started;

        // Apply stored intent
        if self.play_state == PbPlayState::Playing {
            self.bridge.play();
        }
        self.notify_player_status().await;
    }

    /// Player state changed.
    pub async fn on_status_updated(&mut self, player_state: PlayerState, engine_gen: u64) {
        if engine_gen != self.engine_gen {
            return;
        }
        match player_state {
            PlayerState::Playing => self.play_state = PbPlayState::Playing,
            PlayerState::Paused => self.play_state = PbPlayState::Paused,
            PlayerState::Buffering | PlayerState::Idle => {}
        }
        self.notify_player_status().await;
    }

    /// Player progress update.
    pub async fn on_progress_updated(
        &mut self,
        progress_ms: u64,
        duration_ms: u64,
        engine_gen: u64,
    ) {
        if engine_gen != self.engine_gen {
            return;
        }
        self.last_progress_ms = progress_ms;
        if duration_ms > 0 {
            self.current_duration_ms = duration_ms;
        }
        // Throttle progress broadcasts to at most one per second
        if self.last_progress_broadcast.elapsed() >= std::time::Duration::from_secs(1) {
            self.last_progress_broadcast = std::time::Instant::now();
            self.notify_player_status().await;
        }
    }

    /// The local decoder measured the track's real length. `NotifyMediaChanged` is the only
    /// message carrying a length: correcting one means sending it a second time.
    /// It is sent only when the figure actually changes: a repeat the controller did not need
    /// is a repeat it might render.
    pub async fn on_duration_measured(
        &mut self,
        duration_ms: u64,
        track_id: Option<&str>,
        engine_gen: u64,
    ) {
        if engine_gen != self.engine_gen {
            return;
        }
        // A promotion already named the track it moved to, and the adoption that installs it
        // is still in flight. A measurement naming that track belongs to the promotion, not to
        // the track `current_media` still names: held here, the adoption applies it. Announcing
        // it now would charge one track's length to another.
        if let Some(pending) = self.pending_promotion.as_mut()
            && pending.engine_gen == engine_gen
            && crate::ipc::player::same_track(track_id, Some(pending.track_id.as_str()))
        {
            pending.measured_duration_ms = Some(duration_ms);
            return;
        }
        let corrected = {
            let Some(media) = self.current_media.as_mut() else {
                return;
            };
            // Both sides are Connect's own id space, the measurement having been seeded by
            // `speaker_bridge` from this same `track_id()`. Two ids match or nothing does.
            if !crate::ipc::player::same_track(track_id, media.track_id().as_deref()) {
                return;
            }
            let metadata = media.metadata.get_or_insert(MediaMetadata {
                title: None,
                album_title: None,
                artists: None,
                duration: None,
                images: None,
            });
            if metadata.duration == Some(duration_ms) {
                false
            } else {
                metadata.duration = Some(duration_ms);
                true
            }
        };
        if !corrected {
            return;
        }
        self.current_duration_ms = duration_ms;
        self.notify_media_changed().await;
    }

    /// Track finished playing.
    pub async fn on_playback_completed(&mut self, engine_gen: u64) {
        if engine_gen != self.engine_gen {
            return;
        }
        self.state = PbState::Completed;
        self.notify_player_status().await;

        // Check if gapless next media is valid
        if let Some(next) = self.next_media.take() {
            if next.queue_gen == self.queue_gen {
                // Queue unchanged - use preloaded next
                self.set_media(next.media, next.media_seq_no).await;
                return;
            }
            crate::vprintln!("[connect::playback] Next media invalidated: queue changed");
        }

        // No valid next - ask QueueManager
        let _ = self
            .notify_tx
            .send(PlaybackNotifyEvent::Internal(
                PlaybackInternalEvent::MediaCompleted { has_next: false },
            ))
            .await;
    }

    /// A local crossfade promoted the next track, which is already audible.
    ///
    /// The sibling of `on_playback_completed`, minus the only thing that would be wrong here:
    /// preparing the track. The state stays whatever it was (the speaker never stopped) and the
    /// queue moves its index without loading, or the next transition computes `idx + 1` from the
    /// outgoing track and reloads the one already playing.
    ///
    /// `engine_gen` is read, never bumped: nothing about a local fade starts a new engine
    /// session, and bumping it would strand every later event from this track as stale.
    /// `track_id` names what was promoted, so that the length the player measures in the same
    /// breath has something to be judged against before the adoption installs it.
    ///
    /// Records the promotion and returns; the caller adopts in the same turn. Handing the
    /// adoption to the queue through an internal event put two loop checkpoints between the
    /// promotion and the track becoming current, and a `next` landing in between stepped from
    /// the outgoing track, restarting the one already audible.
    pub fn on_crossfade_transitioned(&mut self, engine_gen: u64, track_id: Option<&str>) {
        if engine_gen != self.engine_gen {
            return;
        }
        // A promotion that cannot name its track holds nothing. `same_track` refuses two
        // unidentified sides; an unnamed slot could only ever be matched by guessing, and a
        // length charged to the wrong track is worse than a length not corrected.
        self.pending_promotion = track_id.map(|id| PendingPromotion {
            engine_gen,
            track_id: id.to_string(),
            measured_duration_ms: None,
        });
    }

    /// Take the track the speaker moved to on its own as the current one, and tell the
    /// controller. No `prepare`: it is already playing.
    ///
    /// A length measured back at promotion time is applied before the single
    /// `NotifyMediaChanged` this sends. Correcting one afterwards costs a second message, and
    /// a repeat the controller did not need is a repeat it might render.
    pub async fn adopt_media(&mut self, mut media: MediaInfo, media_seq_no: u32) {
        // Only a length measured for the track this adoption actually names belongs to it. An
        // adoption naming a different item gets nothing (the outcome an unmatched measurement
        // already produces), rather than one track's figure charged to another.
        let engine_gen = self.engine_gen;
        let measured = self
            .pending_promotion
            .take()
            .filter(|promotion| {
                promotion.engine_gen == engine_gen
                    && crate::ipc::player::same_track(
                        Some(promotion.track_id.as_str()),
                        media.track_id().as_deref(),
                    )
            })
            .and_then(|promotion| promotion.measured_duration_ms);
        if let Some(duration_ms) = measured {
            media
                .metadata
                .get_or_insert(MediaMetadata {
                    title: None,
                    album_title: None,
                    artists: None,
                    duration: None,
                    images: None,
                })
                .duration = Some(duration_ms);
        }
        self.current_media = Some(media);
        self.media_seq_no = media_seq_no;
        self.current_duration_ms = measured.unwrap_or(0);
        self.notify_media_changed().await;
    }

    /// Playback error.
    pub async fn on_playback_error(&mut self, status_code: &str, engine_gen: u64) {
        if engine_gen != self.engine_gen {
            return;
        }
        crate::vprintln!("[connect::playback] Error: {}", status_code);

        let notification = PlaybackNotification::NotifyPlaybackError {
            error_code: None,
            status_code: Some(status_code.to_string()),
        };
        if let Ok(msg) = serde_json::to_value(&notification) {
            let _ = self
                .notify_tx
                .send(PlaybackNotifyEvent::Broadcast(msg))
                .await;
        }

        self.state = PbState::Stopped;
        self.notify_player_status().await;
    }

    // ── Queries ──────────────────────────────────────────────────────

    // ── Notifications ────────────────────────────────────────────────

    async fn notify_player_status(&self) {
        let player_state = match (self.state, self.play_state) {
            (PbState::Started, PbPlayState::Playing) => PlayerState::Playing,
            (PbState::Started, PbPlayState::Paused) => PlayerState::Paused,
            (PbState::Preparing, _) => PlayerState::Buffering,
            _ => PlayerState::Idle,
        };

        crate::vprintln!(
            "[connect::playback] notify status: {:?} progress={}ms (state={:?} play={:?})",
            player_state,
            self.last_progress_ms,
            self.state,
            self.play_state
        );

        let msg = serde_json::json!({
            "command": "notifyPlayerStatusChanged",
            "playerState": player_state,
            "progress": self.last_progress_ms,
        });
        {
            let _ = self
                .notify_tx
                .send(PlaybackNotifyEvent::Broadcast(msg))
                .await;
        }
    }

    async fn notify_media_changed(&self) {
        if let Some(ref media) = self.current_media {
            // The native parser takes all 7 fields as fixed types: a null srcUrl or
            // customData fails it.
            let mut broadcast_media = media.clone();
            if broadcast_media.src_url.is_none() {
                broadcast_media.src_url = Some(String::new());
            }
            match &broadcast_media.custom_data {
                Some(serde_json::Value::String(_)) => {}
                Some(v) => {
                    broadcast_media.custom_data = Some(serde_json::Value::String(v.to_string()));
                }
                None => {
                    broadcast_media.custom_data = Some(serde_json::Value::String("{}".to_string()));
                }
            }
            if broadcast_media.metadata.is_none() {
                broadcast_media.metadata = Some(MediaMetadata {
                    title: Some("Unknown".to_string()),
                    album_title: None,
                    artists: None,
                    duration: None,
                    images: None,
                });
            }
            let notification = PlaybackNotification::NotifyMediaChanged {
                media_info: broadcast_media,
            };
            if let Ok(msg) = serde_json::to_value(&notification) {
                let _ = self
                    .notify_tx
                    .send(PlaybackNotifyEvent::Broadcast(msg))
                    .await;
            }
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/connect/receiver/playback.rs"]
mod tests;
