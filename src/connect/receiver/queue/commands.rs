//! Mobile-client command handlers (`proc_*`).
//!
//! These are the per-command bodies dispatched from
//! [`super::QueueManager::dispatch`]. Splitting them into their own file
//! keeps `mod.rs` focused on the state machine, channel wiring, and HTTP
//! orchestration while the command semantics live here.
//!
//! Methods stay on `QueueManager` via a separate `impl` block: Rust allows
//! a type's implementation to be spread across any number of files inside
//! the same module. Visibility is `pub(super)` so `dispatch` in `mod.rs`
//! can call them, but nothing outside the queue module can.

use super::media::resolve_media_url;
use super::next_media_seq;
use super::{
    MediaInfo, QueueChangeReason, QueueInfo, QueueManager, QueueNotification, QueueNotifyEvent,
    QueueState, RepeatMode, http,
};

impl QueueManager {
    pub(super) async fn proc_load_cloud_queue(&mut self, payload: &serde_json::Value) {
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

    pub(super) async fn proc_load_media(&mut self, payload: &serde_json::Value) {
        crate::vprintln!(
            "[connect::queue] loadMedia: hasMediaInfo={}",
            payload.get("mediaInfo").is_some()
        );
        self.state = QueueState::Loading;
        self.bump_queue_gen().await;

        if let Some(mi) = payload.get("mediaInfo")
            && let Ok(mut media) = serde_json::from_value::<MediaInfo>(mi.clone())
        {
            resolve_media_url(&self.http, self.content_server.as_ref(), &mut media).await;
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

    pub(super) async fn proc_select_queue_item(&mut self, payload: &serde_json::Value) {
        self.state = QueueState::Moving;
        self.bump_queue_gen().await;

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

    pub(super) async fn proc_refresh_queue(&mut self, _payload: &serde_json::Value) {
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

    pub(super) async fn proc_set_repeat_mode(&mut self, payload: &serde_json::Value) {
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

        // POST to queue server.
        if let Some(ref qs) = self.queue_server {
            let url = format!("{}/repeatmode", qs.server_url);
            let body = serde_json::json!({ "repeatMode": mode_str });
            if let Err(e) =
                http::post_with_auth(&self.http, self.queue_server.as_ref(), &url, &body).await
            {
                crate::vprintln!("[connect::queue] POST repeatmode failed: {e}");
            }
            let _ = qs;
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

    pub(super) async fn proc_set_shuffle(&mut self, payload: &serde_json::Value) {
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
            if let Err(e) =
                http::post_with_auth(&self.http, self.queue_server.as_ref(), &url, &body).await
            {
                crate::vprintln!("[connect::queue] POST shuffle failed: {e}");
            }
            let _ = qs;
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
    }
}
