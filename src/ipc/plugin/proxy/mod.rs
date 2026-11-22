use super::{ipc_callback_err, ipc_callback_ok, take_ipc_callback};
use crate::app_state::{IpcCallback, IpcMessage, with_state};

/// Restrict proxy channels to Tidal domains only.
/// Returns true (and sends an IPC error) if the URL is rejected.
fn reject_non_tidal(
    url: &crate::ui::nav::RequestUrl,
    channel: &str,
    callback: &IpcCallback,
) -> bool {
    if !crate::ui::nav::is_secure_tidal_target(url) {
        crate::vprintln!(
            "[PROXY]  REJECTED {} to non-Tidal or plaintext URL: {}",
            channel,
            crate::util::truncate_str(&crate::util::redact_url_query(url.as_str()), 80)
        );
        ipc_callback_err(
            callback,
            403,
            &format!("{channel}: only https Tidal URLs are proxied"),
        );
        return true;
    }
    false
}

pub(super) fn handle_proxy_head_dispatch(msg: &IpcMessage, query_id: i64, callback: IpcCallback) {
    let url = crate::ui::nav::RequestUrl::new(
        msg.args
            .first()
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    );

    if reject_non_tidal(&url, "proxy.head", &callback) {
        return;
    }

    with_state(|state| {
        state.pending_ipc_callbacks.insert(query_id, callback);
    });
    crate::state::rt_handle().spawn(async move {
        handle_proxy_head(query_id, url).await;
    });
}

async fn handle_proxy_head(query_id: i64, url: crate::ui::nav::RequestUrl) {
    let client = &*crate::state::HTTP_CLIENT;
    let result = client.head(url.as_str()).send().await;

    let Some(callback) = take_ipc_callback(query_id) else {
        return;
    };
    match result {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let content_length = resp.content_length().unwrap_or(0);
            let json = serde_json::json!({
                "status": status,
                "contentLength": content_length,
            });
            ipc_callback_ok(&callback, &json.to_string());
        }
        Err(e) => {
            ipc_callback_err(&callback, 500, &format!("proxy.head failed: {e}"));
        }
    }
}
