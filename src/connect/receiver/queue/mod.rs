mod auth;
mod commands;
mod error;
mod http;
mod media;
mod window;

pub(crate) use error::QueueError;

use media::{queue_item_to_media_info, resolve_media_url};

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use super::outbox::Outbox;
use crate::connect::token_state::AuthStore;
use crate::connect::types::{
    MediaInfo, QueueChangeReason, QueueInfo, QueueItem, QueueNotification, RepeatMode,
    RequestResult, ServerInfo,
};
use crate::player::LoadOrigin;
use crate::player::ipc::ListenerAct;

/// Internal state of the queue façade. Kept private to the module, and every
/// transition goes through a `QueueManager` method: external callers can
/// observe effects through `QueueNotifyEvent`, but cannot mutate state
/// directly. Sub-modules (`http.rs`, `media.rs`) take data by reference and
/// never touch this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueState {
    Idle,
    Loading,
    Moving,
    Loaded,
}

pub(crate) enum QueueNotifyEvent {
    /// For the live session's phones alone.
    ToSession(serde_json::Value),
    SetMedia {
        media_info: MediaInfo,
        media_seq_no: u32,
    },
    SetNextMedia {
        media_info: MediaInfo,
        media_seq_no: u32,
    },
    SetVolume {
        level: f64,
    },
    SetMute {
        mute: bool,
    },
    QueueGenBumped {
        queue_gen: u64,
    },
}

/// What the receiver makes of a load the SDK on this host asked for during a cast (see
/// `QueueManager::cast_route`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CastRoute {
    /// A listener's pick: play the window's item at `index`, as the controller's own
    /// `selectQueueItem` would.
    Select { index: usize },
    /// The track the cast already plays: the load settles as one that ran, and nothing changes.
    Playing,
    /// A listener's Next: the cast steps as the controller's own `next` makes it. The load is not
    /// run, since it only reloads the entry the page's queue holds.
    Next,
    /// A listener's Previous, as the controller's own `previous`.
    Previous,
    /// A listener's pick the cast does not take: the window does not hold it, or no window is
    /// loaded. TIDAL has already made it the page's queue, which a Play after the cast would
    /// resume: the track the cast plays is announced to the page again, and its queue follows it
    /// back.
    RefusePick,
    /// None of these: an automatic load of another track or with no window loaded, or any load
    /// once the cast is over.
    Refuse,
}

impl CastRoute {
    /// The renderer's answer to load `load_id`, owed exactly once whatever the route: the SDK
    /// waits on it with no timeout. Settled as run when the cast plays that track, refused
    /// otherwise (a step only reloads the page's entry, and is not run either).
    pub(crate) fn answer(self, load_id: u64) -> crate::bridge::PlayerBridgeEvent {
        use crate::bridge::PlayerBridgeEvent;
        match self {
            Self::Select { .. } | Self::Playing => PlayerBridgeEvent::cast_routed(load_id),
            Self::Next | Self::Previous | Self::RefusePick | Self::Refuse => {
                PlayerBridgeEvent::cast_owns_player(Some(load_id))
            }
        }
    }
}

/// A step through the controller's queue, whoever takes it: the controller's own `next` or
/// `previous`, or a listener's on this host during a cast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueStep {
    Next,
    Previous,
}

pub(crate) struct QueueManager {
    state: QueueState,
    queue_gen: u64,

    http: reqwest::Client,

    content_server: Option<ServerInfo>,
    queue_server: Option<ServerInfo>,

    /// Linearized auth state. Seeded from the first ServerInfo that carries
    /// OAuth parameters; mutations during refresh go through RefreshGuard
    /// which keeps concurrent writers from producing torn state. The ServerInfo
    /// fields above are kept in sync and remain the source used to build
    /// outgoing HTTP Authorization headers.
    auth: Option<Arc<AuthStore>>,

    queue_info: Option<QueueInfo>,
    queue_items: Vec<QueueItem>,
    current_index: Option<usize>,
    /// media_id of the currently playing track (from currentMediaInfo).
    /// Used to resolve current_index by matching against queue items' media_id.
    current_media_id_hint: Option<String>,
    /// item_id (UUID) of the currently playing track (from currentMediaInfo).
    /// Used for centering queue window requests around the current item.
    current_item_id_hint: Option<String>,

    outbox: Outbox,

    /// Pending request for retry after token refresh (single retry only)
    pending_retry: Option<PendingRetry>,
    /// Exactly-once gate for client notifications on terminal auth failure.
    /// Reset whenever `sync_auth_from_server_info` installs a fresh generation
    /// (login / relogin): a new generation re-arms the notification.
    terminal_notified: bool,
}

struct PendingRetry {
    url: String,
}

impl QueueManager {
    pub fn new(http: reqwest::Client, outbox: Outbox) -> Self {
        Self {
            state: QueueState::Idle,
            queue_gen: 0,
            http,
            content_server: None,
            queue_server: None,
            auth: None,
            queue_info: None,
            queue_items: Vec::new(),
            current_index: None,
            current_media_id_hint: None,
            current_item_id_hint: None,
            outbox,
            pending_retry: None,
            terminal_notified: false,
        }
    }

    /// The session this queue was loaded for is over, and TIDAL's receiver drops its queue then.
    /// Kept, the window, index and hints steer the next session's steps and loads, and the
    /// servers carry the ended phone's tokens. The generation goes on counting.
    pub fn end_session(&mut self) {
        *self = Self {
            queue_gen: self.queue_gen,
            ..Self::new(self.http.clone(), self.outbox.clone())
        };
    }

    /// Dispatch a command from a mobile client, and say what came of it.
    ///
    /// A command acting on the controller's queue is refused while none is loaded, as TIDAL's
    /// receiver refuses it in its IDLE state. None waits for one: TIDAL's keeps a step back only
    /// until a window load it started lands, and here every load ends inside the call that
    /// started it, none being in flight between two calls. A command kept back would play after
    /// some later load, or never.
    pub async fn handle_command(
        &mut self,
        socket_id: u32,
        command: &str,
        payload: &serde_json::Value,
    ) -> RequestResult {
        match command {
            "loadCloudQueue" => self.proc_load_cloud_queue(payload).await,
            "loadMedia" | "loadMediaInfo" => self.proc_load_media(payload).await,
            "refreshQueue" => self.proc_refresh_queue(payload).await,
            "updateServerInfo" => self.proc_update_server_info(payload).await,
            "next" => return self.step(QueueStep::Next).await,
            "previous" => return self.step(QueueStep::Previous).await,
            "selectQueueItem" | "setRepeatMode" | "setShuffle" if !self.queue_loaded() => {
                return self.refuse(command);
            }
            "selectQueueItem" => self.proc_select_queue_item(payload).await,
            "setRepeatMode" => self.proc_set_repeat_mode(payload).await,
            "setShuffle" => self.proc_set_shuffle(payload).await,
            // The speaker's, not the queue's: TIDAL's receiver answers them from another
            // component, whatever state its queue is in.
            "setVolume" => {
                if let Some(level) = payload.get("level").and_then(|v| v.as_f64()) {
                    self.outbox.push(QueueNotifyEvent::SetVolume { level });
                }
            }
            "setMute" => {
                if let Some(mute) = payload.get("mute").and_then(|v| v.as_bool()) {
                    self.outbox.push(QueueNotifyEvent::SetMute { mute });
                }
            }
            _ => {
                crate::vprintln!(
                    "[connect::queue] Unknown command: {} (socket {})",
                    command,
                    socket_id
                );
            }
        }
        RequestResult::Done
    }

    /// A step through the controller's queue, whoever asks: its own `next` or `previous`, or a
    /// listener's during a cast. Refused with no queue loaded, as every command on the queue.
    pub async fn step(&mut self, step: QueueStep) -> RequestResult {
        if !self.queue_loaded() {
            return self.refuse(&format!("{step:?}"));
        }
        match step {
            QueueStep::Next => self.skip_next().await,
            QueueStep::Previous => self.skip_previous().await,
        }
        RequestResult::Done
    }

    /// Idle is a load that failed or never began. Loading, between two calls, is one that ended
    /// without a window: none is ever in flight there (see `handle_command`).
    fn queue_loaded(&self) -> bool {
        !matches!(self.state, QueueState::Idle | QueueState::Loading)
    }

    fn refuse(&self, command: &str) -> RequestResult {
        crate::vprintln!(
            "[connect::queue] Refusing {}: no queue loaded (state={:?})",
            command,
            self.state
        );
        RequestResult::NoQueue
    }

    /// PlaybackController signals track ended.
    pub async fn on_media_ended(&mut self) {
        if let Some(idx) = self.current_index {
            // Check repeat mode first
            if let Some(ref qi) = self.queue_info
                && qi.repeat_mode == RepeatMode::One
            {
                self.send_set_media(idx).await;
                return;
            }

            self.advance_to_next(idx).await;
        }
    }

    /// Controller sends "next" - skip to next track (ignores repeat-one).
    async fn skip_next(&mut self) {
        crate::vprintln!(
            "[connect::queue] skip_next: current_index={:?} queue_items={}",
            self.current_index,
            self.queue_items.len()
        );
        if let Some(idx) = self.current_index {
            self.advance_to_next(idx).await;
        }
    }

    /// What a load the SDK on this host asked for becomes while the controller owns the player.
    ///
    /// A listener's act plays as the controller's own would: a pick selects its item (the one track
    /// change a receiver may make), Next/Previous step the queue. A pick outside the window is
    /// refused, a receiver never adding to the controller's queue; `CastRoute` names the rest.
    ///
    /// `owner` is read in the routing loop, the only place a cast is released: selecting for a
    /// session already gone would claim the player back.
    ///
    /// A pick searches from the current item and wraps: of two copies, the one the controller
    /// reaches first wins, and picking the playing track restarts it, as TIDAL does.
    pub fn cast_route(
        &self,
        media_id: &str,
        act: Option<ListenerAct>,
        owner: LoadOrigin,
    ) -> CastRoute {
        if owner != LoadOrigin::Connect {
            return CastRoute::Refuse;
        }
        match act {
            // A step reads no window here: it goes through the gate the controller's own takes
            // (`step`), which refuses it with none loaded.
            Some(ListenerAct::Next) => CastRoute::Next,
            Some(ListenerAct::Previous) => CastRoute::Previous,
            // Refused as a pick with no window loaded too: the page made it its queue whatever
            // state the window is in.
            Some(ListenerAct::Pick) if self.state != QueueState::Loaded => CastRoute::RefusePick,
            None if self.state != QueueState::Loaded => CastRoute::Refuse,
            None => {
                let playing = self
                    .playing_item()
                    .is_some_and(|item| item.media_id == media_id);
                if playing {
                    CastRoute::Playing
                } else {
                    CastRoute::Refuse
                }
            }
            Some(ListenerAct::Pick) => {
                let len = self.queue_items.len();
                let start = self.current_index.unwrap_or(0);
                (0..len)
                    .map(|step| (start + step) % len)
                    .find(|&i| self.queue_items[i].media_id == media_id)
                    .map_or(CastRoute::RefusePick, |index| CastRoute::Select { index })
            }
        }
    }

    fn playing_item(&self) -> Option<&QueueItem> {
        self.current_index.and_then(|i| self.queue_items.get(i))
    }

    /// Play the window's item at `index` for a listener's pick, by the steps `selectQueueItem`
    /// takes for the controller's own. The controller hears of it as it hears of its own:
    /// through the `notifyMediaChanged` the load sends.
    pub async fn select_pick(&mut self, index: usize) {
        self.state = QueueState::Moving;
        self.bump_queue_gen();
        self.current_index = Some(index);
        self.send_set_media(index).await;
        self.state = QueueState::Loaded;
        self.reload_queue_window_around_current().await;
    }

    /// Advance from `idx` to the next track, wrapping with RepeatAll.
    async fn advance_to_next(&mut self, idx: usize) {
        let next_idx = idx + 1;
        if next_idx < self.queue_items.len() {
            self.current_index = Some(next_idx);
            self.send_set_media(next_idx).await;
            self.maybe_extend_queue_window().await;
        } else if let Some(ref qi) = self.queue_info
            && qi.repeat_mode == RepeatMode::All
            && !self.queue_items.is_empty()
        {
            self.current_index = Some(0);
            self.send_set_media(0).await;
        } else {
            crate::vprintln!("[connect::queue] End of queue");
        }
    }

    /// Send SetMedia for the item at the given queue index.
    async fn send_set_media(&mut self, idx: usize) {
        if let Some(item) = self.queue_items.get(idx) {
            self.current_media_id_hint = Some(item.media_id.clone());
            self.current_item_id_hint = Some(item.item_id.clone());
            let mut media = queue_item_to_media_info(item);
            resolve_media_url(&self.http, self.content_server.as_ref(), &mut media).await;
            let seq = next_media_seq();
            self.outbox.push(QueueNotifyEvent::SetMedia {
                media_info: media,
                media_seq_no: seq,
            });
        }
    }

    /// Controller sends "previous" - skip to previous track.
    async fn skip_previous(&mut self) {
        if let Some(idx) = self.current_index {
            if idx > 0 {
                self.current_index = Some(idx - 1);
            } else if let Some(ref qi) = self.queue_info
                && qi.repeat_mode == RepeatMode::All
                && !self.queue_items.is_empty()
            {
                self.current_index = Some(self.queue_items.len() - 1);
            } else {
                // Already at start, replay current
                self.current_index = Some(0);
            }
            if let Some(new_idx) = self.current_index {
                self.send_set_media(new_idx).await;
            }
        }
    }

    /// PlaybackController requests next media for gapless.
    pub async fn on_request_next_media(&mut self) {
        if let Some(idx) = self.current_index {
            let next_idx = idx + 1;
            if next_idx < self.queue_items.len() {
                let media = queue_item_to_media_info(&self.queue_items[next_idx]);
                let seq = next_media_seq();
                self.outbox.push(QueueNotifyEvent::SetNextMedia {
                    media_info: media,
                    media_seq_no: seq,
                });
            }
        }
    }

    // Command handlers (`proc_*`) live in `commands.rs` as a separate
    // `impl QueueManager` block to keep this file focused on the state
    // machine and orchestration.

    fn bump_queue_gen(&mut self) {
        self.queue_gen += 1;
        self.outbox.push(QueueNotifyEvent::QueueGenBumped {
            queue_gen: self.queue_gen,
        });
    }

    fn current_item_id(&self) -> Option<String> {
        self.current_index
            .and_then(|idx| self.queue_items.get(idx))
            .map(|item| item.item_id.clone())
            .or_else(|| self.current_item_id_hint.clone())
    }

    fn notify_queue_server_error(&self, error_code: u32, message: &str) {
        let notification = QueueNotification::NotifyQueueServerError {
            error_code,
            details: Some(serde_json::json!({ "description": message })),
        };
        if let Ok(msg) = serde_json::to_value(&notification) {
            self.outbox.push(QueueNotifyEvent::ToSession(msg));
        }
    }
}

fn next_media_seq() -> u32 {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
#[path = "../../../../tests/unit/connect/receiver/queue.rs"]
mod tests;
