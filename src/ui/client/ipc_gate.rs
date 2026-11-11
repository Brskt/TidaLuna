use crate::app_state::IpcMessage;
use crate::ipc::player::handle_ipc_message;
use crate::ipc::plugin::{handle_plugin_ipc, ipc_callback_err, ipc_callback_ok};
use crate::ui::nav::PageKind;
use cef::wrapper::message_router::BrowserSideHandler;
use cef::*;
use std::sync::{Arc, Mutex};

/// True unless the frame is `External` (untrusted origin). `cefQuery` reaches every
/// frame: privileged channels are gated here in Rust, not by JS wrapper isolation.
fn frame_is_trusted(frame: &Option<Frame>, memo: &Mutex<Option<(String, bool)>>) -> bool {
    let Some(frame) = frame else {
        return false;
    };
    let url = crate::ui::token_filter::userfree_to_string(&frame.url());
    trusted_via_memo(&mut memo.lock().unwrap_or_else(|e| e.into_inner()), url)
}

/// A hit requires the freshly fetched URL to match byte-for-byte;
/// interleaving frames can only thrash the memo, never get a stale verdict.
fn trusted_via_memo(memo: &mut Option<(String, bool)>, url: String) -> bool {
    if let Some((cached_url, verdict)) = memo.as_ref()
        && *cached_url == url
    {
        return *verdict;
    }
    let verdict = PageKind::classify(&url).is_trusted();
    *memo = Some((url, verdict));
    verdict
}

/// Channels that mutate auth/session/plugin state or expose a host capability -
/// refused from untrusted ingresses. Single source of truth for the `cefQuery` path,
/// the `__IPC__` console bridge, and `ipc::player` log-redaction; page-chrome stays open.
pub(crate) fn is_privileged_channel(channel: &str) -> bool {
    channel.starts_with("jsrt.")
        || channel.starts_with("connect.")
        || channel.starts_with("updater.")
        || channel.starts_with("settings.")
        || channel.starts_with("plugin.")
        || channel.starts_with("proxy.")
        || channel.starts_with("tidal.")
        || channel.starts_with("__Luna.")
        || channel.starts_with("__LunaNative.")
        || channel.starts_with("player.devices.")
        || channel == "player.parse_dash"
        || channel == "window.navigate_self"
        || channel == "window.open_url"
}

/// The channel in a form safe to log. `__LunaNative.<token>` carries the capability reaching one
/// registered native module, and the redaction contract covers args only: without this the token
/// lands in the persistent sink, where pasting a log into an issue hands out a working handle.
pub(crate) fn log_safe_channel(channel: &str) -> &str {
    if channel.starts_with("__LunaNative.") {
        return "__LunaNative.<token>";
    }
    channel
}

/// Channels answered by their handler. Any other invoke is answered "ok" before its handler runs,
/// which a page waiting on the reply would take for the work being done.
fn is_invoke_channel(channel: &str) -> bool {
    channel.starts_with("plugin.")
        || channel.starts_with("proxy.")
        || channel.starts_with("jsrt.")
        || channel.starts_with("tidal.")
        || channel.starts_with("__Luna.")
        || channel.starts_with("__LunaNative.")
        || channel.starts_with("updater.")
        || channel.starts_with("connect.")
        || channel == "player.parse_dash"
        || channel == "settings.video_codec_state"
        || channel == "settings.set_video_codec"
        || channel == "settings.audio_cache_state"
        || channel == "settings.set_audio_cache_enabled"
        || channel == "settings.set_audio_cache_size"
        || channel == "settings.plugin_update_at_boot"
        || channel == "settings.store_repair"
}

/// Privileged-frame gate decision: an untrusted frame's privileged channel is refused
/// (403 if a reply is awaited, else a silent ack). Applied uniformly: a channel
/// cannot slip the gate by being absent from the dispatch list.
#[derive(Debug, PartialEq, Eq)]
enum PrivilegedGate {
    Allow,
    Refuse403,
    DropAck,
}

fn privileged_gate(privileged: bool, trusted: bool, has_id: bool) -> PrivilegedGate {
    if privileged && !trusted {
        if has_id {
            PrivilegedGate::Refuse403
        } else {
            PrivilegedGate::DropAck
        }
    } else {
        PrivilegedGate::Allow
    }
}

#[derive(Default)]
pub(in crate::ui) struct IpcQueryHandler {
    // Mutex only because the shared handler's Sync bound demands it;
    // on_query_str is UI-thread-only; the lock never contends.
    frame_trust_memo: Mutex<Option<(String, bool)>>,
}

impl BrowserSideHandler for IpcQueryHandler {
    fn on_query_str(
        &self,
        _browser: Option<Browser>,
        frame: Option<Frame>,
        query_id: i64,
        request: &str,
        _persistent: bool,
        callback: Arc<Mutex<dyn cef::wrapper::message_router::BrowserSideCallback>>,
    ) -> bool {
        if let Ok(msg) = serde_json::from_str::<IpcMessage>(request) {
            // Privileged channels: trusted frame only. Refuse req-resp (403); drop
            // fire-and-forget with an ack (no consumer, but the cefQuery must resolve).
            match privileged_gate(
                is_privileged_channel(&msg.channel),
                frame_is_trusted(&frame, &self.frame_trust_memo),
                msg.id.is_some(),
            ) {
                PrivilegedGate::Refuse403 => {
                    ipc_callback_err(&callback, 403, "IPC restricted to TIDAL frames");
                    return true;
                }
                PrivilegedGate::DropAck => {
                    crate::vprintln!(
                        "[IPC]    Dropped privileged fire-and-forget from untrusted frame: {}",
                        log_safe_channel(&msg.channel)
                    );
                    ipc_callback_ok(&callback, "ok");
                    return true;
                }
                PrivilegedGate::Allow => {}
            }

            // `connect.*` is main-frame-only: a subframe must not drive the controller.
            // Req-resp gets 403; fire-and-forget is dropped with an ack.
            if msg.channel.starts_with("connect.")
                && frame.as_ref().map(|f| f.is_main() == 0).unwrap_or(true)
            {
                if msg.id.is_some() {
                    ipc_callback_err(&callback, 403, "Connect IPC restricted to main frame");
                } else {
                    crate::vprintln!(
                        "[IPC]    Dropped connect.* fire-and-forget from non-main frame: {}",
                        msg.channel
                    );
                    ipc_callback_ok(&callback, "ok");
                }
                return true;
            }

            // Req-resp plugin/connect dispatch (trusted by the gate above).
            if msg.id.is_some() && is_invoke_channel(&msg.channel) {
                if msg.channel.starts_with("connect.") {
                    crate::connect::ipc::handle_connect_invoke(msg, callback);
                } else {
                    handle_plugin_ipc(msg, query_id, callback);
                }
                return true;
            }
        }

        handle_ipc_message(request);
        ipc_callback_ok(&callback, "ok");
        true
    }

    /// CEF's contract: once a query is cancelled, explicitly or by browser destruction,
    /// navigation or a renderer crash, no reference to its callback may be kept and none of
    /// its methods may run.
    ///
    /// Honoured here only for the five channels that register in `pending_ipc_callbacks`, the
    /// ones holding a callback across an unbounded `reqwest` call. Every other async handler
    /// moves its callback into the spawned future or parks it in `plugin_load_waiters`, and
    /// `connect.*` cannot be reached at all without giving `handle_connect_invoke` the
    /// `query_id`; those rely on the wrapper's `detach()` plus a `Weak` making a late
    /// `success`/`failure` a silent no-op. What none of it stops is work nobody awaits still
    /// running, a download permit among it.
    fn on_query_canceled(&self, _browser: Option<Browser>, _frame: Option<Frame>, query_id: i64) {
        crate::ipc::plugin::drop_ipc_callback(query_id);
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/ui/client/ipc_gate.rs"]
mod tests;
