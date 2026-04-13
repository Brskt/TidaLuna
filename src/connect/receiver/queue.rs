use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::mpsc;

use crate::connect::types::{
    MediaInfo, QueueChangeReason, QueueInfo, QueueItem, QueueNotification, RepeatMode, ServerInfo,
    consts,
};
use base64::Engine as _;

// ── State machine ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueState {
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

// ── HTTP error ───────────────────────────────────────────────────────

enum QueueHttpError {
    TokenExpired,
    Other(String),
}

// ── QueueManager ─────────────────────────────────────────────────────

pub(crate) struct QueueManager {
    state: QueueState,
    queue_gen: u64,

    http: reqwest::Client,

    content_server: Option<ServerInfo>,
    queue_server: Option<ServerInfo>,

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
            queue_info: None,
            queue_items: Vec::new(),
            current_index: None,
            current_media_id_hint: None,
            current_item_id_hint: None,
            notify_tx,
            pending_retry: None,
            deferred_commands: Vec::new(),
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
            self.resolve_media_url(&mut media).await;
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

    async fn proc_load_cloud_queue(&mut self, payload: &serde_json::Value) {
        self.state = QueueState::Loading;
        self.bump_queue_gen().await;
        self.deferred_commands.clear();

        let has_content = payload.get("contentServerInfo").is_some();
        let has_queue = payload.get("queueServerInfo").is_some();
        let has_qi = payload.get("queueInfo").is_some();
        let has_media = payload.get("currentMediaInfo").is_some();
        crate::vprintln!(
            "[connect::queue] loadCloudQueue: contentSI={} queueSI={} queueInfo={} currentMedia={}",
            has_content,
            has_queue,
            has_qi,
            has_media
        );

        if let Some(content) = payload.get("contentServerInfo") {
            self.content_server = serde_json::from_value(content.clone()).ok();
        }
        if let Some(queue) = payload.get("queueServerInfo") {
            self.queue_server = serde_json::from_value(queue.clone()).ok();
        }
        if let Some(qi) = payload.get("queueInfo") {
            crate::vprintln!("[connect::queue] queueInfo raw: {}", qi);
            match serde_json::from_value::<QueueInfo>(qi.clone()) {
                Ok(parsed) => {
                    crate::vprintln!(
                        "[connect::queue] queueInfo parsed OK: id={}",
                        parsed.queue_id
                    );
                    self.queue_info = Some(parsed);
                }
                Err(e) => {
                    crate::vprintln!("[connect::queue] queueInfo parse FAILED: {}", e);
                }
            }
        }

        // Extract currentMediaInfo hints before loading the queue window,
        // so process_queue_window_response can resolve current_index immediately.
        let current_item_id = payload
            .get("currentMediaInfo")
            .and_then(|c| c.get("itemId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if let Some(ref id) = current_item_id {
            self.current_item_id_hint = Some(id.clone());
        }
        if let Some(mid) = payload
            .get("currentMediaInfo")
            .and_then(|c| c.get("mediaId"))
            .and_then(|v| v.as_str())
        {
            self.current_media_id_hint = Some(mid.to_string());
        }

        // Load queue window centered on the current item
        if let Some(ref qi) = self.queue_info {
            let before = qi.max_before_size;
            let after = qi.max_after_size;
            crate::vprintln!(
                "[connect::queue] Loading queue window (before={}, after={}, itemId={:?})",
                before,
                after,
                current_item_id
            );
            self.load_queue_window(current_item_id.as_deref(), before, after)
                .await;
            crate::vprintln!(
                "[connect::queue] Queue window loaded: {} items",
                self.queue_items.len()
            );
        }

        // If a currentMediaInfo is provided, send it to PlaybackController directly
        if let Some(current) = payload.get("currentMediaInfo") {
            crate::vprintln!("[connect::queue] currentMediaInfo raw: {}", current);
            match serde_json::from_value::<MediaInfo>(current.clone()) {
                Ok(mut media) => {
                    crate::vprintln!(
                        "[connect::queue] currentMediaInfo parsed: id={} media={}",
                        media.item_id,
                        media.media_id
                    );
                    self.current_media_id_hint = Some(media.media_id.clone());
                    self.current_item_id_hint = Some(media.item_id.clone());
                    self.resolve_media_url(&mut media).await;
                    let seq = next_media_seq();
                    let _ = self
                        .notify_tx
                        .send(QueueNotifyEvent::SetMedia {
                            media_info: media,
                            media_seq_no: seq,
                        })
                        .await;
                }
                Err(e) => {
                    crate::vprintln!("[connect::queue] currentMediaInfo parse FAILED: {}", e);
                }
            }
        }
    }

    async fn proc_load_media(&mut self, payload: &serde_json::Value) {
        crate::vprintln!(
            "[connect::queue] loadMedia: hasMediaInfo={}",
            payload.get("mediaInfo").is_some()
        );
        self.state = QueueState::Loading;
        self.bump_queue_gen().await;

        if let Some(mi) = payload.get("mediaInfo")
            && let Ok(mut media) = serde_json::from_value::<MediaInfo>(mi.clone())
        {
            self.resolve_media_url(&mut media).await;
            let seq = next_media_seq();
            self.state = QueueState::Loaded;
            let _ = self
                .notify_tx
                .send(QueueNotifyEvent::SetMedia {
                    media_info: media,
                    media_seq_no: seq,
                })
                .await;
        }
    }

    async fn proc_select_queue_item(&mut self, payload: &serde_json::Value) {
        self.state = QueueState::Moving;
        self.bump_queue_gen().await;

        if let Some(mi) = payload.get("mediaInfo")
            && let Ok(mut media) = serde_json::from_value::<MediaInfo>(mi.clone())
        {
            // Find index in current queue
            if let Some(idx) = self
                .queue_items
                .iter()
                .position(|item| item.item_id == media.item_id)
            {
                self.current_index = Some(idx);
            }

            self.resolve_media_url(&mut media).await;
            let seq = next_media_seq();
            self.state = QueueState::Loaded;
            let _ = self
                .notify_tx
                .send(QueueNotifyEvent::SetMedia {
                    media_info: media,
                    media_seq_no: seq,
                })
                .await;

            self.reload_queue_window_around_current().await;
        }
    }

    async fn proc_refresh_queue(&mut self, _payload: &serde_json::Value) {
        crate::vprintln!(
            "[connect::queue] refreshQueue: queueInfo={:?}, currentItemId={:?}",
            self.queue_info.is_some(),
            self.current_item_id()
        );
        self.state = QueueState::Loading;
        self.bump_queue_gen().await;

        if let Some(ref qi) = self.queue_info {
            self.load_queue_window(
                self.current_item_id().as_deref(),
                qi.max_before_size,
                qi.max_after_size,
            )
            .await;
        }

        let notification = QueueNotification::NotifyQueueItemsChanged {
            queue_info: self.queue_info.clone().unwrap_or_else(|| QueueInfo {
                queue_id: String::new(),
                repeat_mode: RepeatMode::None,
                shuffled: false,
                max_after_size: 0,
                max_before_size: 0,
            }),
            reason: Some(QueueChangeReason::RefreshRequested),
        };
        if let Ok(msg) = serde_json::to_value(&notification) {
            let _ = self.notify_tx.send(QueueNotifyEvent::Broadcast(msg)).await;
        }
    }

    async fn proc_set_repeat_mode(&mut self, payload: &serde_json::Value) {
        self.bump_queue_gen().await;

        let mode_str = payload
            .get("repeatMode")
            .and_then(|v| v.as_str())
            .unwrap_or("NONE");
        let mode = match mode_str {
            "ONE" => RepeatMode::One,
            "ALL" => RepeatMode::All,
            _ => RepeatMode::None,
        };

        if let Some(ref mut qi) = self.queue_info {
            qi.repeat_mode = mode;
        }

        // POST to queue server
        if let Some(ref qs) = self.queue_server {
            let url = format!("{}/repeatmode", qs.server_url);
            let body = serde_json::json!({ "repeatMode": mode_str });
            self.http_post_with_auth(&url, &body).await;
        }

        let notification = QueueNotification::NotifyQueueItemsChanged {
            queue_info: self.queue_info.clone().unwrap_or_else(|| QueueInfo {
                queue_id: String::new(),
                repeat_mode: mode,
                shuffled: false,
                max_after_size: 0,
                max_before_size: 0,
            }),
            reason: Some(QueueChangeReason::RepeatModeChanged),
        };
        if let Ok(msg) = serde_json::to_value(&notification) {
            let _ = self.notify_tx.send(QueueNotifyEvent::Broadcast(msg)).await;
        }
    }

    async fn proc_set_shuffle(&mut self, payload: &serde_json::Value) {
        self.bump_queue_gen().await;

        let shuffle = payload
            .get("shuffle")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if let Some(ref mut qi) = self.queue_info {
            qi.shuffled = shuffle;
        }

        if let Some(ref qs) = self.queue_server {
            let url = format!("{}/shuffle", qs.server_url);
            let body = serde_json::json!({ "shuffle": shuffle });
            self.http_post_with_auth(&url, &body).await;
        }

        let notification = QueueNotification::NotifyQueueItemsChanged {
            queue_info: self.queue_info.clone().unwrap_or_else(|| QueueInfo {
                queue_id: String::new(),
                repeat_mode: RepeatMode::None,
                shuffled: shuffle,
                max_after_size: 0,
                max_before_size: 0,
            }),
            reason: Some(QueueChangeReason::ShuffleChanged),
        };
        if let Ok(msg) = serde_json::to_value(&notification) {
            let _ = self.notify_tx.send(QueueNotifyEvent::Broadcast(msg)).await;
        }
    }

    async fn proc_update_server_info(&mut self, payload: &serde_json::Value) {
        crate::vprintln!(
            "[connect::queue] updateServerInfo: contentSI={} queueSI={}",
            payload.get("contentServerInfo").is_some(),
            payload.get("queueServerInfo").is_some()
        );
        if let Some(content) = payload.get("contentServerInfo") {
            self.content_server = serde_json::from_value(content.clone()).ok();
        }
        if let Some(queue) = payload.get("queueServerInfo") {
            self.queue_server = serde_json::from_value(queue.clone()).ok();
        }

        // Retry pending request if token was refreshed
        if let Some(pending) = self.pending_retry.take() {
            self.retry_pending(&pending.url).await;
        }
    }

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
        match self.http_get_with_auth(&url).await {
            Ok(response) => {
                crate::vprintln!("[connect::queue] Queue window response OK");
                self.process_queue_window_response(response).await;
            }
            Err(QueueHttpError::TokenExpired) => {
                crate::vprintln!("[connect::queue] Queue window 401 - refreshing token");
                self.pending_retry = Some(PendingRetry { url });
                self.refresh_token().await;
            }
            Err(QueueHttpError::Other(e)) => {
                crate::vprintln!("[connect::queue] Queue window fetch failed: {}", e);
                self.state = QueueState::Idle;
                self.notify_queue_server_error(0, &e).await;
            }
        }
    }

    async fn http_get_with_auth(&self, url: &str) -> Result<serde_json::Value, QueueHttpError> {
        let server = self
            .queue_server
            .as_ref()
            .ok_or(QueueHttpError::Other("No server".into()))?;
        let auth_header = resolve_auth_header(server);

        let response = self
            .http
            .get(url)
            .header("Authorization", &auth_header)
            .timeout(std::time::Duration::from_secs(consts::HTTP_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|e| QueueHttpError::Other(e.to_string()))?;

        if response.status().as_u16() == 401 {
            return Err(QueueHttpError::TokenExpired);
        }
        if !response.status().is_success() {
            return Err(QueueHttpError::Other(format!("HTTP {}", response.status())));
        }

        response
            .json::<serde_json::Value>()
            .await
            .map_err(|e| QueueHttpError::Other(e.to_string()))
    }

    async fn http_post_with_auth(&self, url: &str, body: &serde_json::Value) {
        let server = match &self.queue_server {
            Some(s) => s,
            None => return,
        };
        let auth_header = resolve_auth_header(server);

        let _ = self
            .http
            .post(url)
            .header("Authorization", &auth_header)
            .json(body)
            .timeout(std::time::Duration::from_secs(consts::HTTP_TIMEOUT_SECS))
            .send()
            .await;
    }

    /// Refresh OAuth token. Direct POST - no retry of the refresh itself.
    async fn refresh_token(&mut self) {
        let oauth = self
            .content_server
            .as_ref()
            .or(self.queue_server.as_ref())
            .and_then(|s| s.auth_info.as_ref())
            .and_then(|a| a.oauth_server_info.as_ref());

        let oauth = match oauth {
            Some(o) => o.clone(),
            None => {
                crate::vprintln!("[connect::queue] No OAuth server info for token refresh");
                self.pending_retry = None;
                return;
            }
        };

        let auth_header = oauth
            .auth_info
            .header_auth
            .as_deref()
            .unwrap_or("")
            .to_string();

        let mut form = std::collections::HashMap::new();
        if let Some(ref fp) = oauth.form_parameters {
            form.insert("grant_type".to_string(), fp.grant_type.clone());
            form.insert("scope".to_string(), fp.scope.clone());
        }
        if let Some(ref params) = oauth.auth_info.oauth_parameters {
            form.insert("refresh_token".to_string(), params.refresh_token.clone());
        }

        let result: Result<reqwest::Response, reqwest::Error> = self
            .http
            .post(&oauth.server_url)
            .header("Authorization", &auth_header)
            .form(&form)
            .timeout(std::time::Duration::from_secs(consts::HTTP_TIMEOUT_SECS))
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    let token_val: Option<&str> = body.get("access_token").and_then(|v| v.as_str());
                    if let Some(new_token) = token_val {
                        self.update_access_token(new_token);
                        crate::vprintln!("[connect::queue] Token refreshed");

                        // Retry pending request
                        if let Some(pending) = self.pending_retry.take() {
                            self.retry_pending(&pending.url).await;
                        }
                        return;
                    }
                }
                crate::vprintln!("[connect::queue] Token refresh response missing access_token");
            }
            Ok(resp) => {
                crate::vprintln!(
                    "[connect::queue] Token refresh failed: HTTP {}",
                    resp.status()
                );
            }
            Err(e) => {
                crate::vprintln!("[connect::queue] Token refresh request failed: {}", e);
            }
        }

        // Refresh failed - give up on pending retry
        self.pending_retry = None;
        self.state = QueueState::Idle;
        self.notify_queue_server_error(401, "Token refresh failed")
            .await;
    }

    fn update_access_token(&mut self, new_token: &str) {
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
    }

    async fn retry_pending(&mut self, url: &str) {
        match self.http_get_with_auth(url).await {
            Ok(response) => self.process_queue_window_response(response).await,
            Err(e) => {
                let msg = match e {
                    QueueHttpError::TokenExpired => "Token still expired after refresh",
                    QueueHttpError::Other(ref s) => s.as_str(),
                };
                crate::vprintln!("[connect::queue] Retry failed: {}", msg);
                self.state = QueueState::Idle;
            }
        }
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
                crate::vprintln!("[connect::queue] First item keys: {:?}", first.as_object().map(|o| o.keys().collect::<Vec<_>>()));
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
                self.queue_items.first().map(|i| i.item_id.as_str()).unwrap_or("?"),
                self.queue_items.first().map(|i| i.media_id.as_str()).unwrap_or("?")
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

    /// Resolve a mediaId to a streaming URL via the TIDAL playbackinfo API.
    /// Fills in srcUrl, stream_type, and encryption key on the MediaInfo.
    async fn resolve_media_url(&self, media: &mut MediaInfo) {
        if media.src_url.is_some() {
            return; // Already resolved
        }

        let cs = match &self.content_server {
            Some(s) => s,
            None => {
                crate::vprintln!("[connect::queue] No content server for media resolution");
                return;
            }
        };

        let auth_header = resolve_auth_header(cs);
        let quality = cs
            .query_parameters
            .get("audioquality")
            .and_then(|v| v.as_str())
            .unwrap_or("HI_RES_LOSSLESS");

        let url = format!(
            "{}/tracks/{}/playbackinfo?audioquality={}&playbackmode=STREAM&assetpresentation=FULL",
            cs.server_url, media.media_id, quality
        );

        crate::vprintln!(
            "[connect::queue] Resolving media {} via {}",
            media.media_id,
            url
        );

        let resp = match self
            .http
            .get(&url)
            .header("Authorization", &auth_header)
            .timeout(std::time::Duration::from_secs(consts::HTTP_TIMEOUT_SECS))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                crate::vprintln!("[connect::queue] playbackinfo HTTP {}", r.status());
                return;
            }
            Err(e) => {
                crate::vprintln!("[connect::queue] playbackinfo request failed: {}", e);
                return;
            }
        };

        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                crate::vprintln!("[connect::queue] playbackinfo parse failed: {}", e);
                return;
            }
        };


        let manifest_b64 = match body.get("manifest").and_then(|v| v.as_str()) {
            Some(m) => m,
            None => {
                crate::vprintln!("[connect::queue] playbackinfo: no manifest field");
                return;
            }
        };

        let manifest_mime = body
            .get("manifestMimeType")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let manifest_bytes = match base64::engine::general_purpose::STANDARD.decode(manifest_b64) {
            Ok(b) => b,
            Err(e) => {
                crate::vprintln!("[connect::queue] manifest base64 decode failed: {}", e);
                return;
            }
        };

        if manifest_mime == "application/dash+xml" {
            let manifest_str = String::from_utf8_lossy(&manifest_bytes).to_string();
            crate::vprintln!(
                "[connect::queue] DASH manifest ({} bytes)",
                manifest_str.len()
            );
            match crate::player::dash::parse_dash_mpd(&manifest_str) {
                Ok(dash) => {
                    media.src_url = Some(dash.init_url);
                    media.stream_type = Some(
                        dash.codec
                            .split('.')
                            .next()
                            .unwrap_or(&dash.codec)
                            .to_string(),
                    );
                    // custom_data may be a JSON string (from mobile), replace with object
                    let cd = media
                        .custom_data
                        .get_or_insert_with(|| serde_json::json!({}));
                    if !cd.is_object() {
                        *cd = serde_json::json!({});
                    }
                    cd["dashSegmentUrls"] =
                        serde_json::to_value(&dash.segment_urls).unwrap_or_default();
                    // Inject duration from DASH manifest if metadata doesn't have it
                    if let Some(secs) = dash.duration_secs {
                        if let Some(ref mut md) = media.metadata {
                            if md.duration.is_none() {
                                md.duration = Some((secs * 1000.0) as u64);
                            }
                        }
                    }
                    crate::vprintln!(
                        "[connect::queue] DASH resolved: {} segments",
                        dash.segment_urls.len()
                    );
                }
                Err(e) => {
                    crate::vprintln!("[connect::queue] DASH parse failed: {}", e);
                }
            }
            return;
        }

        // BTS/JSON manifest (FLAC, lossless)
        let manifest: serde_json::Value = match serde_json::from_slice(&manifest_bytes) {
            Ok(v) => v,
            Err(e) => {
                crate::vprintln!("[connect::queue] manifest JSON parse failed: {}", e);
                return;
            }
        };

        if let Some(urls) = manifest.get("urls").and_then(|v| v.as_array())
            && let Some(stream_url) = urls.first().and_then(|v| v.as_str())
        {
            media.src_url = Some(stream_url.to_string());
            crate::vprintln!("[connect::queue] Resolved: {}", stream_url);
        }
        if let Some(codecs) = manifest.get("codecs").and_then(|v| v.as_str()) {
            let base_codec = codecs.split('.').next().unwrap_or(codecs);
            media.stream_type = Some(base_codec.to_string());
        }
        if let Some(key_id) = manifest.get("keyId").and_then(|v| v.as_str())
            && !key_id.is_empty()
        {
            let cd = media
                .custom_data
                .get_or_insert_with(|| serde_json::json!({}));
            if !cd.is_object() {
                *cd = serde_json::json!({});
            }
            cd["encryptionKey"] = serde_json::Value::String(key_id.to_string());
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

fn queue_item_to_media_info(item: &QueueItem) -> MediaInfo {
    // Build metadata from item.metadata, falling back to display_info fields.
    // Cloud queue items often lack `metadata` but have `display_info` with title/artists/images.
    let mut metadata: Option<crate::connect::types::MediaMetadata> = item
        .metadata
        .as_ref()
        .and_then(|m| serde_json::from_value(m.clone()).ok());

    if let Some(ref di) = item.display_info {
        let md = metadata.get_or_insert(crate::connect::types::MediaMetadata {
            title: None,
            album_title: None,
            artists: None,
            duration: None,
            images: None,
        });
        if md.title.is_none() {
            md.title = di.title.clone();
        }
        if md.album_title.is_none() {
            md.album_title = di.album_title.clone();
        }
        if md.artists.is_none() {
            md.artists = di.artists.clone();
        }
        if md.images.is_none() {
            md.images = di.images.clone();
        }
        if md.duration.is_none() {
            md.duration = di.duration.or(item.duration_ms);
        }
    }

    MediaInfo {
        item_id: item.item_id.clone(),
        media_id: item.media_id.clone(),
        src_url: item.src_url.clone(),
        stream_type: None,
        metadata,
        custom_data: item.custom_data.clone(),
        media_type: 0,
        policy: crate::connect::types::default_policy(),
    }
}

fn resolve_auth_header(server: &ServerInfo) -> String {
    if let Some(ref ai) = server.auth_info {
        if let Some(ref ha) = ai.header_auth {
            return ha.clone();
        }
        if let Some(ref params) = ai.oauth_parameters {
            return format!("Bearer {}", params.access_token);
        }
        if let Some(ref oauth_server) = ai.oauth_server_info {
            if let Some(ref ha) = oauth_server.auth_info.header_auth {
                return ha.clone();
            }
            if let Some(ref params) = oauth_server.auth_info.oauth_parameters {
                return format!("Bearer {}", params.access_token);
            }
        }
    }
    String::new()
}
