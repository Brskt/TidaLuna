use super::{ipc_callback_err, ipc_callback_ok, take_ipc_callback};
use crate::app_state::{IpcCallback, IpcMessage, with_state};
use cef::ImplCookieManager;

fn parse_set_cookie(header: &str) -> Option<cef::Cookie> {
    // Format: "name=value; Path=/; Domain=.tidal.com; Secure; HttpOnly; ..."
    let mut parts = header.splitn(2, ';');
    let name_value = parts.next()?;
    let (name, value) = name_value.split_once('=')?;
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() {
        return None;
    }

    let mut domain = String::new();
    let mut path = String::from("/");
    let mut secure = 0;
    let mut httponly = 0;

    if let Some(attrs) = parts.next() {
        for attr in attrs.split(';') {
            let attr = attr.trim();
            if let Some((k, v)) = attr.split_once('=') {
                match k.trim().to_lowercase().as_str() {
                    "domain" => domain = v.trim().to_string(),
                    "path" => path = v.trim().to_string(),
                    _ => {}
                }
            } else {
                match attr.to_lowercase().as_str() {
                    "secure" => secure = 1,
                    "httponly" => httponly = 1,
                    _ => {}
                }
            }
        }
    }

    Some(cef::Cookie {
        size: std::mem::size_of::<cef::Cookie>(),
        name: cef::CefString::from(name),
        value: cef::CefString::from(value),
        domain: cef::CefString::from(domain.as_str()),
        path: cef::CefString::from(path.as_str()),
        secure,
        httponly,
        creation: cef::Basetime { val: 0 },
        last_access: cef::Basetime { val: 0 },
        has_expires: 0,
        expires: cef::Basetime { val: 0 },
        same_site: Default::default(),
        priority: Default::default(),
    })
}

pub(super) fn handle_proxy_fetch_dispatch(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg
        .args
        .first()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let opts_json = msg
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
            handle_proxy_fetch(id, url, opts_json).await;
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

async fn handle_proxy_fetch(id: String, url: String, opts_json: String) {
    let client = &*crate::state::HTTP_CLIENT;

    let opts: serde_json::Map<String, serde_json::Value> = if !opts_json.is_empty() {
        serde_json::from_str(&opts_json).unwrap_or_default()
    } else {
        Default::default()
    };

    let method = opts.get("method").and_then(|v| v.as_str()).unwrap_or("GET");

    let mut req = match method {
        "POST" => client.post(&url),
        "PUT" => client.put(&url),
        "PATCH" => client.patch(&url),
        "DELETE" => client.delete(&url),
        "HEAD" => client.head(&url),
        _ => client.get(&url),
    };

    if let Some(headers_str) = opts.get("headers").and_then(|v| v.as_str())
        && let Ok(headers) =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(headers_str)
    {
        for (key, value) in headers {
            if let Some(val) = value.as_str() {
                req = req.header(&key, val);
            }
        }
    }

    if let Some(body) = opts.get("body").and_then(|v| v.as_str()) {
        req = req.body(body.to_string());
    }

    // Inject captured token for TIDAL API requests missing Authorization.
    let has_auth = opts
        .get("headers")
        .and_then(|v| v.as_str())
        .map(|h| h.contains("Authorization"))
        .unwrap_or(false);
    if !has_auth && (url.contains("api.tidal.com") || url.contains("desktop.tidal.com")) {
        let token = with_state(|state| state.captured_token.clone()).unwrap_or_default();
        if !token.is_empty() {
            req = req.header("Authorization", format!("Bearer {token}"));
            crate::vprintln!(
                "[PROXY]  Injected captured token for {}",
                &url[..url.len().min(80)]
            );
        }
    }

    let result = req.send().await;

    let Some(callback) = take_ipc_callback(&id) else {
        return;
    };
    match result {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let mut headers_map = serde_json::Map::new();
            let mut set_cookies: Vec<String> = Vec::new();
            for (name, value) in resp.headers().iter() {
                if let Ok(v) = value.to_str() {
                    if name == reqwest::header::SET_COOKIE {
                        set_cookies.push(v.to_string());
                    }
                    headers_map.insert(
                        name.as_str().to_string(),
                        serde_json::Value::String(v.to_string()),
                    );
                }
            }
            // Mirror Set-Cookie to CEF's cookie jar (JS can't — forbidden header).
            if !set_cookies.is_empty() {
                crate::vprintln!(
                    "[PROXY]  Mirroring {} Set-Cookie header(s) for {}",
                    set_cookies.len(),
                    &url[..url.len().min(80)]
                );
                if let Some(cm) = cef::cookie_manager_get_global_manager(None) {
                    let cef_url = cef::CefString::from(url.as_str());
                    for cookie_str in &set_cookies {
                        if let Some(cookie) = parse_set_cookie(cookie_str) {
                            cm.set_cookie(Some(&cef_url), Some(&cookie), None);
                        }
                    }
                }
            }
            let is_token_endpoint = url.contains("/oauth2/token");
            let is_4xx = (400..500).contains(&(status as u32));
            let has_auth = opts
                .get("headers")
                .and_then(|v| v.as_str())
                .map(|h| h.contains("Authorization"))
                .unwrap_or(false);
            tokio::spawn(async move {
                let body = resp.text().await.unwrap_or_default();
                if is_4xx {
                    crate::vprintln!(
                        "[PROXY]  {} {} auth={} body={}",
                        status,
                        &url[..url.len().min(200)],
                        has_auth,
                        &body[..body.len().min(400)]
                    );
                }
                // Log auth token endpoint responses (no secrets)
                if is_token_endpoint {
                    let error = serde_json::from_str::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|v| {
                            let err = v.get("error")?.as_str()?.to_string();
                            let desc = v
                                .get("error_description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("");
                            Some(format!("{err}: {desc}"))
                        });
                    match error {
                        Some(e) => {
                            crate::vprintln!("[AUTH]   /oauth2/token → {} error={}", status, e)
                        }
                        None => crate::vprintln!("[AUTH]   /oauth2/token → {}", status),
                    }
                }
                let json = serde_json::json!({
                    "status": status,
                    "body": body,
                    "headers": headers_map,
                });
                ipc_callback_ok(&callback, &json.to_string());
            });
        }
        Err(e) => {
            ipc_callback_err(&callback, &format!("proxy.fetch failed: {e}"));
        }
    }
}
