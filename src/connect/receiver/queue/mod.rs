mod commands;
mod error;
mod http;
mod media;

pub(crate) use error::QueueError;

use media::{queue_item_to_media_info, resolve_media_url};

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::connect::token_state::{
    AuthStore, GenerationStatus, RefreshGuard, TerminationReason, TokenState,
};
use crate::connect::types::{
    MediaInfo, QueueChangeReason, QueueInfo, QueueItem, QueueNotification, RepeatMode, ServerInfo,
};

// ── State machine ────────────────────────────────────────────────────

/// Internal state of the queue façade. Kept private to the module so every
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

// ── Events emitted ───────────────────────────────────────────────────

pub(crate) enum QueueNotifyEvent {
    Broadcast(serde_json::Value),
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

// ── Deferred command ────────────────────────────────────────────────

struct DeferredCommand {
    socket_id: u32,
    command: String,
    payload: serde_json::Value,
}

const MAX_DEFERRED: usize = 32;

// ── QueueManager ─────────────────────────────────────────────────────

pub(crate) struct QueueManager {
    state: QueueState,
    queue_gen: u64,

    http: reqwest::Client,

    content_server: Option<ServerInfo>,
    queue_server: Option<ServerInfo>,

    /// Linearized auth state. Seeded from the first ServerInfo that carries
    /// OAuth parameters; mutations during refresh go through RefreshGuard
    /// so concurrent writers cannot produce torn state. The ServerInfo
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

    notify_tx: mpsc::Sender<QueueNotifyEvent>,

    /// Pending request for retry after token refresh (single retry only)
    pending_retry: Option<PendingRetry>,
    /// Commands deferred during Loading state
    deferred_commands: Vec<DeferredCommand>,
    /// Exactly-once gate for client notifications on terminal auth failure.
    /// Reset whenever `sync_auth_from_server_info` installs a fresh generation
    /// (login / relogin), so a new generation re-arms the notification.
    terminal_notified: bool,
}

struct PendingRetry {
    url: String,
}

impl QueueManager {
    pub fn new(http: reqwest::Client, notify_tx: mpsc::Sender<QueueNotifyEvent>) -> Self {
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
            notify_tx,
            pending_retry: None,
            deferred_commands: Vec::new(),
            terminal_notified: false,
        }
    }

    /// Extract access/refresh tokens from the current ServerInfos and seed or
    /// advance the AuthStore. Called whenever the mobile client pushes
    /// `loadCloudQueue` or `updateServerInfo`.
    ///
    /// The wire format keeps tokens in both `auth_info.oauth_parameters` and
    /// optionally in `oauth_server_info.auth_info.oauth_parameters`. Prefer
    /// the outer copy; fall back to the inner one.
    fn sync_auth_from_server_info(&mut self) {
        let params = http::extract_oauth_params(self.content_server.as_ref())
            .or_else(|| http::extract_oauth_params(self.queue_server.as_ref()));
        let Some((access_token, refresh_token, scope)) = params else {
            return;
        };

        // A fresh generation (login / relogin) re-arms the one-shot terminal
        // notification so the next `invalid_grant` produces exactly one
        // outgoing notification instead of being silently suppressed.
        self.terminal_notified = false;

        match self.auth.as_ref() {
            None => {
                let initial = TokenState {
                    generation: 1,
                    token_version: 1,
                    access_token,
                    refresh_token: Some(refresh_token),
                    scope,
                    expires_at: Instant::now() + Duration::from_secs(3600),
                    status: GenerationStatus::Active,
                };
                self.auth = Some(Arc::new(AuthStore::new(initial)));
            }
            Some(store) => {
                // The wire pushed a new set of tokens (relogin or quality
                // change). Bump the generation so any in-flight refresh
                // guard tied to the previous snapshot will fail its CAS.
                // Uses `store` (not CAS) because the wire is authoritative:
                // a previously-terminated generation must not block the new
                // tokens from being installed.
                let prev = store.load();
                let mut next = (*prev).clone();
                next.generation = prev.generation + 1;
                next.token_version = 1;
                next.access_token = access_token;
                next.refresh_token = Some(refresh_token);
                next.scope = scope;
                next.expires_at = Instant::now() + Duration::from_secs(3600);
                next.status = GenerationStatus::Active;
                store.store(next);
            }
        }
    }

    /// Dispatch a command from a mobile client.
    pub async fn handle_command(
        &mut self,
        socket_id: u32,
        command: &str,
        payload: &serde_json::Value,
    ) {
        // Commands that always execute regardless of state
        match command {
            "loadCloudQueue" | "loadMedia" | "loadMediaInfo" | "updateServerInfo"
            | "refreshQueue" => {
                self.dispatch(socket_id, command, payload).await;
                return;
            }
            _ => {}
        }

        // Commands that require a loaded queue - defer if Loading/Idle
        if self.state == QueueState::Loading || self.state == QueueState::Idle {
            crate::vprintln!(
                "[connect::queue] Deferring command '{}' (state={:?})",
                command,
                self.state
            );
            if self.deferred_commands.len() < MAX_DEFERRED {
                self.deferred_commands.push(DeferredCommand {
                    socket_id,
                    command: command.to_string(),
                    payload: payload.clone(),
                });
            } else {
                crate::vprintln!(
                    "[connect::queue] Deferred queue full - dropping '{}'",
                    command
                );
            }
            return;
        }

        self.dispatch(socket_id, command, payload).await;
    }

    async fn dispatch(&mut self, socket_id: u32, command: &str, payload: &serde_json::Value) {
        match command {
            "loadCloudQueue" => self.proc_load_cloud_queue(payload).await,
            "loadMedia" | "loadMediaInfo" => self.proc_load_media(payload).await,
            "selectQueueItem" => self.proc_select_queue_item(payload).await,
            "refreshQueue" => self.proc_refresh_queue(payload).await,
            "setRepeatMode" => self.proc_set_repeat_mode(payload).await,
            "setShuffle" => self.proc_set_shuffle(payload).await,
            "updateServerInfo" => self.proc_update_server_info(payload).await,
            "setVolume" => {
                if let Some(level) = payload.get("level").and_then(|v| v.as_f64()) {
                    let _ = self
                        .notify_tx
                        .send(QueueNotifyEvent::SetVolume { level })
                        .await;
                }
            }
            "setMute" => {
                if let Some(mute) = payload.get("mute").and_then(|v| v.as_bool()) {
                    let _ = self
                        .notify_tx
                        .send(QueueNotifyEvent::SetMute { mute })
                        .await;
                }
            }
            "next" => self.skip_next().await,
            "previous" => self.skip_previous().await,
            _ => {
                crate::vprintln!(
                    "[connect::queue] Unknown command: {} (socket {})",
                    command,
                    socket_id
                );
            }
        }
    }

    // ── Internal callbacks ───────────────────────────────────────────

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
            let _ = self
                .notify_tx
                .send(QueueNotifyEvent::SetMedia {
                    media_info: media,
                    media_seq_no: seq,
                })
                .await;
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
                let _ = self
                    .notify_tx
                    .send(QueueNotifyEvent::SetNextMedia {
                        media_info: media,
                        media_seq_no: seq,
                    })
                    .await;
            }
        }
    }

    // ── Command handlers ─────────────────────────────────────────────

    // Command handlers (`proc_*`) live in `commands.rs` as a separate
    // `impl QueueManager` block to keep this file focused on the state
    // machine and orchestration.

    // ── HTTP ─────────────────────────────────────────────────────────

    async fn load_queue_window(
        &mut self,
        item_id: Option<&str>,
        before_size: u32,
        after_size: u32,
    ) {
        let qs = match &self.queue_server {
            Some(qs) => qs.clone(),
            None => {
                crate::vprintln!("[connect::queue] No queue server info");
                self.state = QueueState::Idle;
                return;
            }
        };

        let queue_id = self
            .queue_info
            .as_ref()
            .map(|qi| qi.queue_id.as_str())
            .unwrap_or("");
        let mut url = format!(
            "{}/{}/window?beforesize={}&aftersize={}&onlyactives=true",
            qs.server_url, queue_id, before_size, after_size
        );
        if let Some(id) = item_id {
            url.push_str(&format!("&itemid={}", id));
        }

        crate::vprintln!("[connect::queue] HTTP GET {}", url);
        match http::get_with_auth(&self.http, self.queue_server.as_ref(), &url).await {
            Ok(response) => {
                crate::vprintln!("[connect::queue] Queue window response OK");
                self.process_queue_window_response(response).await;
            }
            Err(QueueError::TokenExpired) => {
                crate::vprintln!("[connect::queue] Queue window 401 - refreshing token");
                self.pending_retry = Some(PendingRetry { url });
                match self.refresh_token().await {
                    Ok(()) => {
                        // Token refreshed; retry the pending request immediately.
                        if let Some(pending) = self.pending_retry.take() {
                            match self.retry_pending(&pending.url).await {
                                Ok(()) => {}
                                Err(QueueError::TokenExpired) => {
                                    // A 401 on a request made with the freshly-
                                    // minted access token means the server has
                                    // revoked the grant out-of-band. Mark the
                                    // generation Terminated(Revoked) so further
                                    // refresh attempts against this snapshot are
                                    // rejected by `AuthStore::compare_and_swap`,
                                    // and notify the client exactly once.
                                    crate::vprintln!(
                                        "[connect::queue] 401 post-refresh: generation revoked"
                                    );
                                    self.mark_generation_revoked();
                                    self.state = QueueState::Idle;
                                    if !self.terminal_notified {
                                        self.terminal_notified = true;
                                        self.notify_queue_server_error(401, "revoked").await;
                                    }
                                }
                                Err(e) => {
                                    crate::vprintln!(
                                        "[connect::queue] Retry after refresh failed: {e}"
                                    );
                                    self.state = QueueState::Idle;
                                }
                            }
                        }
                    }
                    Err(QueueError::AuthTerminated { provider_error }) => {
                        crate::vprintln!(
                            "[connect::queue] Auth terminated (invalid_grant): {provider_error}"
                        );
                        self.pending_retry = None;
                        self.state = QueueState::Idle;
                        // Exactly-once per terminal transition. `terminal_notified`
                        // is re-armed by `sync_auth_from_server_info` whenever a
                        // fresh generation arrives from the wire.
                        if !self.terminal_notified {
                            self.terminal_notified = true;
                            self.notify_queue_server_error(
                                401,
                                &format!("invalid_grant: {provider_error}"),
                            )
                            .await;
                        }
                    }
                    Err(e) => {
                        crate::vprintln!("[connect::queue] Token refresh failed: {e}");
                        self.pending_retry = None;
                        self.state = QueueState::Idle;
                        self.notify_queue_server_error(401, &format!("{e}")).await;
                    }
                }
            }
            Err(e) => {
                crate::vprintln!("[connect::queue] Queue window fetch failed: {e}");
                self.state = QueueState::Idle;
                self.notify_queue_server_error(0, &format!("{e}")).await;
            }
        }
    }

    /// Refresh OAuth token. The HTTP exchange lives in `http::refresh_token`;
    /// this method is the façade's CAS-plus-sync layer: it takes the current
    /// `AuthStore` snapshot, calls the stateless refresh helper, then
    /// applies the outcome through `RefreshGuard` so a concurrent refresh
    /// cannot produce torn state.
    async fn refresh_token(&mut self) -> Result<(), QueueError> {
        // Static OAuth server configuration (URL, grant_type, header) lives
        // in ServerInfo. The live refresh_token lives in AuthStore.
        let oauth = self
            .content_server
            .as_ref()
            .or(self.queue_server.as_ref())
            .and_then(|s| s.auth_info.as_ref())
            .and_then(|a| a.oauth_server_info.as_ref())
            .cloned()
            .ok_or(QueueError::NoOAuthServer)?;

        let store = self
            .auth
            .as_ref()
            .cloned()
            .ok_or(QueueError::NoOAuthServer)?;
        let guard = RefreshGuard::new(&store);
        let snapshot = guard.snapshot().clone();
        let current_refresh_token = snapshot
            .refresh_token
            .clone()
            .ok_or(QueueError::MissingField("refresh_token"))?;

        let success = match http::refresh_token(&self.http, &oauth, &current_refresh_token).await {
            Ok(s) => s,
            Err(QueueError::AuthTerminated { provider_error }) => {
                self.mark_generation_terminated(&snapshot, &provider_error);
                return Err(QueueError::AuthTerminated { provider_error });
            }
            Err(e) => return Err(e),
        };

        // Build the next auth state from the snapshot so `token_version`
        // only advances once per successful refresh.
        let mut next = (*snapshot).clone();
        next.token_version += 1;
        next.access_token = success.access_token.clone();
        if let Some(rt) = success.refresh_token {
            next.refresh_token = Some(rt);
        }
        next.expires_at = Instant::now() + Duration::from_secs(3600);
        next.status = GenerationStatus::Active;

        guard.try_apply(next).map_err(|cas| match cas {
            crate::connect::token_state::CASError::VersionMismatch => {
                QueueError::InvalidResponse("refresh CAS version mismatch".to_string())
            }
            crate::connect::token_state::CASError::Terminated => {
                QueueError::InvalidResponse("refresh against terminated generation".to_string())
            }
        })?;

        self.update_access_token(&success.access_token)?;
        crate::vprintln!("[connect::queue] Token refreshed");
        Ok(())
    }

    /// Transition the given generation snapshot to
    /// `Terminated(InvalidGrant)`. Uses `AuthStore::store` (unconditional
    /// write) so a terminal state cannot be erased by a later stale refresh
    /// attempt. `suspect_replay` is left false: RFC 9700 says replay detection
    /// is a server-side property that the client cannot determine reliably
    /// from a single response, so the flag remains observational.
    fn mark_generation_terminated(&mut self, snapshot: &Arc<TokenState>, provider_error: &str) {
        let Some(store) = self.auth.as_ref() else {
            return;
        };
        let mut terminated = (**snapshot).clone();
        terminated.status = GenerationStatus::Terminated(TerminationReason::InvalidGrant {
            provider_error: provider_error.to_string(),
            suspect_replay: false,
        });
        store.store(terminated);
    }

    /// Transition the current generation to `Terminated(Revoked)` without a
    /// pre-captured snapshot: used when a request made with a freshly-minted
    /// access token is rejected with 401, implying a server-side revocation
    /// (RFC 7009). `store()` bypasses the CAS so a relogin from the wire can
    /// still install fresh credentials afterwards.
    fn mark_generation_revoked(&mut self) {
        let Some(store) = self.auth.as_ref() else {
            return;
        };
        let mut terminated = (*store.load()).clone();
        terminated.status = GenerationStatus::Terminated(TerminationReason::Revoked);
        store.store(terminated);
    }

    /// Persist a new access token into both content and queue ServerInfos.
    /// The AuthStore is the authoritative source; this keeps the wire-shaped
    /// ServerInfo in sync so outgoing HTTP Authorization headers reflect the
    /// refreshed token.
    fn update_access_token(&mut self, new_token: &str) -> Result<(), QueueError> {
        fn update_server(server: &mut Option<ServerInfo>, token: &str) {
            if let Some(s) = server
                && let Some(ref mut ai) = s.auth_info
            {
                // Update header_auth so resolve_auth_header uses the new token
                if ai.header_auth.is_some() {
                    ai.header_auth = Some(format!("Bearer {}", token));
                }
                if let Some(ref mut params) = ai.oauth_parameters {
                    params.access_token = token.to_string();
                }
                if let Some(ref mut oauth) = ai.oauth_server_info {
                    if oauth.auth_info.header_auth.is_some() {
                        oauth.auth_info.header_auth = Some(format!("Bearer {}", token));
                    }
                    if let Some(ref mut params) = oauth.auth_info.oauth_parameters {
                        params.access_token = token.to_string();
                    }
                }
            }
        }
        update_server(&mut self.content_server, new_token);
        update_server(&mut self.queue_server, new_token);
        Ok(())
    }

    async fn retry_pending(&mut self, url: &str) -> Result<(), QueueError> {
        let response = http::get_with_auth(&self.http, self.queue_server.as_ref(), url).await?;
        self.process_queue_window_response(response).await;
        Ok(())
    }

    // ── Queue window processing ──────────────────────────────────────

    async fn process_queue_window_response(&mut self, response: serde_json::Value) {
        crate::vprintln!(
            "[connect::queue] Processing queue window: keys={:?}",
            response.as_object().map(|o| o.keys().collect::<Vec<_>>())
        );
        if let Some(items) = response
            .get("items")
            .or_else(|| response.get("mediaInfos"))
            .and_then(|v| v.as_array())
        {
            crate::vprintln!("[connect::queue] Raw items count: {}", items.len());
            if let Some(first) = items.first() {
                crate::vprintln!(
                    "[connect::queue] First item keys: {:?}",
                    first.as_object().map(|o| o.keys().collect::<Vec<_>>())
                );
                if let Err(e) = serde_json::from_value::<QueueItem>(first.clone()) {
                    crate::vprintln!("[connect::queue] First item parse FAILED: {}", e);
                }
            }
            self.queue_items = items
                .iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect();
        }

        // Resolve current_index: try response itemId, then hints.
        // The response may include itemId (UUID). We also have the media_id hint from currentMediaInfo.
        let item_id_hint = response
            .get("itemId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| self.current_item_id_hint.clone());
        // Try matching by item_id (UUID) first, then by media_id (track number)
        let found = item_id_hint
            .as_ref()
            .and_then(|cid| {
                self.queue_items
                    .iter()
                    .position(|item| item.item_id == *cid)
            })
            .or_else(|| {
                self.current_media_id_hint.as_ref().and_then(|mid| {
                    self.queue_items
                        .iter()
                        .position(|item| item.media_id == *mid)
                })
            });
        if found.is_some() {
            self.current_index = found;
        }
        {
            crate::vprintln!(
                "[connect::queue] Resolve current_index: item_hint={:?} media_hint={:?} found={:?} first_item_id={} first_media_id={}",
                item_id_hint.as_deref().unwrap_or("?"),
                self.current_media_id_hint.as_deref().unwrap_or("?"),
                found,
                self.queue_items
                    .first()
                    .map(|i| i.item_id.as_str())
                    .unwrap_or("?"),
                self.queue_items
                    .first()
                    .map(|i| i.media_id.as_str())
                    .unwrap_or("?")
            );
        }

        // Update queue info fields if present
        if let Some(ref mut info) = self.queue_info {
            if let Some(qi) = response.get("queueId").and_then(|v| v.as_str()) {
                info.queue_id = qi.to_string();
            }
            if let Some(rm) = response.get("repeatMode").and_then(|v| v.as_str()) {
                info.repeat_mode = match rm {
                    "ONE" => RepeatMode::One,
                    "ALL" => RepeatMode::All,
                    _ => RepeatMode::None,
                };
            }
            if let Some(sh) = response.get("shuffle").and_then(|v| v.as_bool()) {
                info.shuffled = sh;
            }
        }

        self.state = QueueState::Loaded;

        // Notify clients
        if let Some(ref qi) = self.queue_info {
            let notification = QueueNotification::NotifyQueueChanged {
                queue_info: qi.clone(),
            };
            if let Ok(msg) = serde_json::to_value(&notification) {
                let _ = self.notify_tx.send(QueueNotifyEvent::Broadcast(msg)).await;
            }
        }

        // Replay deferred commands
        self.replay_deferred().await;
    }

    fn replay_deferred(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let commands = std::mem::take(&mut self.deferred_commands);
            for cmd in commands {
                crate::vprintln!("[connect::queue] Replaying deferred '{}'", cmd.command);
                self.dispatch(cmd.socket_id, &cmd.command, &cmd.payload)
                    .await;
            }
        })
    }

    // ── Helpers ──────────────────────────────────────────────────────

    async fn bump_queue_gen(&mut self) {
        self.queue_gen += 1;
        let _ = self.notify_tx.try_send(QueueNotifyEvent::QueueGenBumped {
            queue_gen: self.queue_gen,
        });
    }

    fn current_item_id(&self) -> Option<String> {
        self.current_index
            .and_then(|idx| self.queue_items.get(idx))
            .map(|item| item.item_id.clone())
            .or_else(|| self.current_item_id_hint.clone())
    }

    async fn reload_queue_window_around_current(&mut self) {
        if let Some(ref qi) = self.queue_info {
            let before = qi.max_before_size;
            let after = qi.max_after_size;
            self.load_queue_window(self.current_item_id().as_deref(), before, after)
                .await;
        }
    }

    async fn maybe_extend_queue_window(&mut self) {
        if let (Some(idx), Some(qi)) = (self.current_index, &self.queue_info) {
            let remaining = self.queue_items.len().saturating_sub(idx + 1);
            if remaining <= 2 {
                let before = qi.max_before_size;
                let after = qi.max_after_size;
                self.load_queue_window(self.current_item_id().as_deref(), before, after)
                    .await;
            }
        }
    }

    async fn notify_queue_server_error(&self, error_code: u32, message: &str) {
        let notification = QueueNotification::NotifyQueueServerError {
            error_code,
            details: Some(serde_json::json!({ "description": message })),
        };
        if let Ok(msg) = serde_json::to_value(&notification) {
            let _ = self.notify_tx.send(QueueNotifyEvent::Broadcast(msg)).await;
        }
    }
}

// ── Free functions ───────────────────────────────────────────────────

fn next_media_seq() -> u32 {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}
