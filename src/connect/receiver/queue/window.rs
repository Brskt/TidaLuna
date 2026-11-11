use super::{PendingRetry, QueueError, QueueManager, QueueNotifyEvent, QueueState, http};
use crate::connect::types::{QueueItem, QueueNotification, RepeatMode};

impl QueueManager {
    /// Clamp a peer-requested queue-window size; a malicious controller could
    /// otherwise request an enormous window.
    const MAX_QUEUE_WINDOW: u32 = 500;

    pub(super) async fn load_queue_window(
        &mut self,
        item_id: Option<&str>,
        before_size: u32,
        after_size: u32,
    ) {
        let before_size = before_size.min(Self::MAX_QUEUE_WINDOW);
        let after_size = after_size.min(Self::MAX_QUEUE_WINDOW);
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
                                    // generation Terminated(Revoked); further
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
                                        self.notify_queue_server_error(401, "revoked");
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
                            );
                        }
                    }
                    Err(e) => {
                        crate::vprintln!("[connect::queue] Token refresh failed: {e}");
                        self.pending_retry = None;
                        self.state = QueueState::Idle;
                        self.notify_queue_server_error(401, &format!("{e}"));
                    }
                }
            }
            Err(e) => {
                crate::vprintln!("[connect::queue] Queue window fetch failed: {e}");
                self.state = QueueState::Idle;
                self.notify_queue_server_error(0, &format!("{e}"));
            }
        }
    }

    pub(super) async fn retry_pending(&mut self, url: &str) -> Result<(), QueueError> {
        let response = http::get_with_auth(&self.http, self.queue_server.as_ref(), url).await?;
        self.process_queue_window_response(response).await;
        Ok(())
    }

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
            }
            let raw = items.len();
            let mut dropped = 0usize;
            let parsed: Vec<QueueItem> = items
                .iter()
                .filter_map(|v| match serde_json::from_value::<QueueItem>(v.clone()) {
                    Ok(item) => Some(item),
                    Err(e) => {
                        dropped += 1;
                        crate::vprintln!("[connect::queue] Dropped unparseable item: {e}");
                        None
                    }
                })
                .collect();
            // A window where every item failed to parse would otherwise replace a
            // good queue with an empty one and leave current_index dangling; keep
            // the previous queue instead of advancing into nothing.
            if parsed.is_empty() && raw > 0 {
                crate::vprintln!(
                    "[connect::queue] All {raw} items failed to parse; keeping previous queue"
                );
                return;
            }
            if dropped > 0 {
                crate::vprintln!(
                    "[connect::queue] Parsed {} of {raw} items ({dropped} dropped)",
                    parsed.len()
                );
            }
            self.queue_items = parsed;
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
                self.outbox.push(QueueNotifyEvent::ToSession(msg));
            }
        }
    }

    pub(super) async fn reload_queue_window_around_current(&mut self) {
        if let Some(ref qi) = self.queue_info {
            let before = qi.max_before_size;
            let after = qi.max_after_size;
            self.load_queue_window(self.current_item_id().as_deref(), before, after)
                .await;
        }
    }

    pub(super) async fn maybe_extend_queue_window(&mut self) {
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
}
