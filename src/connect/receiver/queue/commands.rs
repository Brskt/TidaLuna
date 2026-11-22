//! Mobile-client command handlers (`proc_*`).
//!
//! These are the per-command bodies dispatched from
//! [`super::QueueManager::dispatch`]. Splitting them into their own file
//! keeps `mod.rs` focused on the state machine, its effects, and HTTP
//! orchestration while the command semantics live here.
//!
//! Methods stay on `QueueManager` via a separate `impl` block: Rust allows
//! a type's implementation to be spread across any number of files inside
//! the same module. Visibility is `pub(super)`: `dispatch` in `mod.rs`
//! can call them, but nothing outside the queue module can.

use super::http::{ConnectRoot, PutReply, RootKind};
use super::media::resolve_media_url;
use super::next_media_seq;
use super::{
    MediaInfo, PendingChange, QueueChange, QueueChangeReason, QueueInfo, QueueManager,
    QueueNotification, QueueNotifyEvent, QueueState, RepeatMode, RequestResult, http,
};

impl QueueManager {
    pub(super) async fn proc_load_cloud_queue(&mut self, payload: &serde_json::Value) {
        self.state = QueueState::Loading;
        self.bump_queue_gen();

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
        self.sync_auth_from_server_info();
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

        // Extract currentMediaInfo hints before loading the queue window;
        // process_queue_window_response resolves current_index immediately.
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

        // Load queue window centered on the current item.
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

        // If a currentMediaInfo is provided, send it to PlaybackController directly.
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
                    resolve_media_url(&self.http, self.content_server.as_ref(), &mut media).await;
                    let seq = next_media_seq();
                    self.outbox.push(QueueNotifyEvent::SetMedia {
                        media_info: media,
                        media_seq_no: seq,
                    });
                }
                Err(e) => {
                    crate::vprintln!("[connect::queue] currentMediaInfo parse FAILED: {}", e);
                }
            }
        }
    }

    pub(super) async fn proc_load_media(&mut self, payload: &serde_json::Value) {
        crate::vprintln!(
            "[connect::queue] loadMedia: hasMediaInfo={}",
            payload.get("mediaInfo").is_some()
        );
        self.state = QueueState::Loading;
        self.bump_queue_gen();

        if let Some(mi) = payload.get("mediaInfo")
            && let Ok(mut media) = serde_json::from_value::<MediaInfo>(mi.clone())
        {
            resolve_media_url(&self.http, self.content_server.as_ref(), &mut media).await;
            let seq = next_media_seq();
            self.state = QueueState::Loaded;
            self.outbox.push(QueueNotifyEvent::SetMedia {
                media_info: media,
                media_seq_no: seq,
            });
        }
    }

    pub(super) async fn proc_select_queue_item(&mut self, payload: &serde_json::Value) {
        self.state = QueueState::Moving;
        self.bump_queue_gen();

        if let Some(mi) = payload.get("mediaInfo")
            && let Ok(mut media) = serde_json::from_value::<MediaInfo>(mi.clone())
        {
            // Find index in current queue.
            if let Some(idx) = self
                .queue_items
                .iter()
                .position(|item| item.item_id == media.item_id)
            {
                self.current_index = Some(idx);
            }

            resolve_media_url(&self.http, self.content_server.as_ref(), &mut media).await;
            let seq = next_media_seq();
            self.state = QueueState::Loaded;
            self.outbox.push(QueueNotifyEvent::SetMedia {
                media_info: media,
                media_seq_no: seq,
            });

            self.reload_queue_window_around_current().await;
        }
    }

    pub(super) async fn proc_refresh_queue(&mut self, _payload: &serde_json::Value) {
        crate::vprintln!(
            "[connect::queue] refreshQueue: queueInfo={:?}, currentItemId={:?}",
            self.queue_info.is_some(),
            self.current_item_id()
        );
        self.state = QueueState::Loading;
        self.bump_queue_gen();

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
                repeat_mode: RepeatMode::Off,
                shuffled: false,
                max_after_size: 0,
                max_before_size: 0,
            }),
            reason: Some(QueueChangeReason::RefreshRequested),
        };
        if let Ok(msg) = serde_json::to_value(&notification) {
            self.outbox.push(QueueNotifyEvent::ToSession(msg));
        }
    }

    /// A controller's shuffle or repeat change, made as TIDAL's receiver makes it: a PUT to the
    /// queue server, then on success `notifyQueueItemsChanged`, a window reload and the answer.
    /// `None` when the answer is queued with those effects; `socket_id` is `None` for a click here.
    pub(super) async fn proc_change(
        &mut self,
        socket_id: Option<u32>,
        request_id: Option<u64>,
        change: QueueChange,
    ) -> Option<RequestResult> {
        self.bump_queue_gen();
        if let Some(replaced) = self.pending_change.take() {
            self.answer(
                replaced.socket_id,
                replaced.request_id,
                RequestResult::Cancelled,
            );
        }
        // A media loaded alone has no cloud queue to change.
        let Some(current) = self.queue_info.clone() else {
            return Some(RequestResult::NoQueue);
        };
        if change.is_in(&current) {
            crate::vprintln!("[connect::queue] {change:?}: already in place");
            return Some(RequestResult::Done);
        }
        let target = change.applied_to(&current);
        let Some(server) = self.queue_server.clone() else {
            crate::verr!("[connect::queue] {change:?}: no queue server");
            return Some(RequestResult::Cancelled);
        };

        let url = match ConnectRoot::admit_logged(&server.server_url, RootKind::Service)
            .and_then(|root| change.url(&root, &current.queue_id))
        {
            Ok(url) => url,
            Err(e) => {
                crate::verr!("[connect::queue] {change:?}: refused, {e}");
                return Some(RequestResult::Cancelled);
            }
        };
        let body = change.body(&self.current_item_id().unwrap_or_default());
        match http::put_with_auth(&self.http, Some(&server), &url, &body).await {
            Ok(PutReply::Applied) => {
                crate::vprintln!("[connect::queue] {change:?}: applied");
                self.queue_info = Some(target.clone());
                let notification = QueueNotification::NotifyQueueItemsChanged {
                    queue_info: target,
                    reason: Some(change.reason()),
                };
                if let Ok(msg) = serde_json::to_value(&notification) {
                    self.outbox.push(QueueNotifyEvent::ToSession(msg));
                }
                self.outbox.push(QueueNotifyEvent::ReloadAfterChange {
                    socket_id,
                    request_id,
                });
                None
            }
            Ok(PutReply::TokenExpired { body }) => {
                crate::vprintln!("[connect::queue] {change:?}: 401, waiting for updateServerInfo");
                // To the phone that asked; for a click on this host, to the session's phones, which
                // alone hold a token to send.
                let message = Self::queue_server_refusal(401, &body);
                self.outbox.push(match socket_id {
                    Some(socket_id) => QueueNotifyEvent::ToSocket { socket_id, message },
                    None => QueueNotifyEvent::ToSession(message),
                });
                self.pending_change = Some(PendingChange {
                    socket_id,
                    request_id,
                    target,
                });
                None
            }
            // Answered before the refusal reaches the session's phones, as TIDAL's receiver
            // answers it.
            Ok(PutReply::Refused { status, body }) => {
                crate::verr!("[connect::queue] {change:?}: refused with HTTP {status}");
                self.outbox
                    .push(QueueNotifyEvent::ToSession(Self::queue_server_refusal(
                        status, &body,
                    )));
                self.drop_queue();
                Some(RequestResult::Cancelled)
            }
            // TIDAL's receiver leaves these unanswered until a newer change cancels them: the
            // phone gets that cancel now instead.
            Ok(PutReply::Unhandled { status }) => {
                crate::verr!("[connect::queue] {change:?}: unexpected HTTP {status}");
                Some(RequestResult::Cancelled)
            }
            Err(e) => {
                crate::verr!("[connect::queue] {change:?}: {e}");
                Some(RequestResult::Cancelled)
            }
        }
    }

    /// The window reload that follows a change the queue server took, then its answer. Run by the
    /// routing loop once the change's `notifyQueueItemsChanged` is out: TIDAL's receiver sends
    /// that notification as soon as the PUT succeeds, ahead of the reload.
    pub async fn reload_after_change(&mut self, socket_id: Option<u32>, request_id: Option<u64>) {
        self.reload_queue_window_around_current().await;
        self.answer_after_reload(socket_id, request_id);
    }

    /// The answer to a change once its window reload ends: done if the window came, cancelled
    /// if the reload dropped the queue.
    fn answer_after_reload(&self, socket_id: Option<u32>, request_id: Option<u64>) {
        let result = if self.queue_loaded() {
            RequestResult::Done
        } else {
            RequestResult::Cancelled
        };
        self.answer(socket_id, request_id, result);
    }

    pub(super) async fn proc_update_server_info(&mut self, payload: &serde_json::Value) {
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
        self.sync_auth_from_server_info();

        // Retry pending request if token was refreshed.
        if let Some(pending) = self.pending_retry.take()
            && let Err(e) = self.retry_pending(&pending.url).await
        {
            crate::vprintln!("[connect::queue] Retry after updateServerInfo failed: {e}");
            self.state = QueueState::Idle;
        }

        // A change refused with a 401 is not sent again: TIDAL's receiver reloads the window
        // with the change applied, then answers it.
        if let Some(pending) = self.pending_change.take() {
            self.queue_info = Some(pending.target);
            self.reload_queue_window_around_current().await;
            self.answer_after_reload(pending.socket_id, pending.request_id);
        }
    }
}
