use super::{ipc_callback_err, ipc_callback_ok, take_ipc_callback};
use crate::app_state::{IpcCallback, IpcMessage, with_state};

pub(super) fn handle_proxy_fetch_dispatch(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let headers_json = msg
        .args
        .get(1)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let id = msg.id.clone().unwrap_or_default();
    with_state(|state| {
        state.pending_ipc_callbacks.insert(id.clone(), callback);
        let rt = state.rt_handle.clone();
        rt.spawn(async move {
            handle_proxy_fetch(id, url, headers_json).await;
        });
    });
}

pub(super) fn handle_proxy_head_dispatch(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let id = msg.id.clone().unwrap_or_default();
    with_state(|state| {
        state.pending_ipc_callbacks.insert(id.clone(), callback);
        let rt = state.rt_handle.clone();
        rt.spawn(async move {
            handle_proxy_head(id, url).await;
        });
    });
}

async fn handle_proxy_head(id: String, url: String) {
    let client = &*crate::state::HTTP_CLIENT;
    let result = client.head(&url).send().await;

    let Some(callback) = take_ipc_callback(&id) else {
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
            ipc_callback_err(&callback, &format!("proxy.head failed: {e}"));
        }
    }
}

async fn handle_proxy_fetch(id: String, url: String, headers_json: String) {
    let client = &*crate::state::HTTP_CLIENT;
    let mut req = client.get(&url);

    if !headers_json.is_empty()
        && let Ok(headers) =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&headers_json)
    {
        for (key, value) in headers {
            if let Some(val) = value.as_str() {
                req = req.header(&key, val);
            }
        }
    }

    let result = req.send().await;

    let Some(callback) = take_ipc_callback(&id) else {
        return;
    };
    match result {
        Ok(resp) => {
            let status = resp.status().as_u16();
            tokio::spawn(async move {
                let body = resp.text().await.unwrap_or_default();
                let json = serde_json::json!({
                    "status": status,
                    "body": body,
                });
                ipc_callback_ok(&callback, &json.to_string());
            });
        }
        Err(e) => {
            ipc_callback_err(&callback, &format!("proxy.fetch failed: {e}"));
        }
    }
}
