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

/// Check if text contains any real OAuth token (access or refresh, current or previous).
/// Defence-in-depth against naive exfiltration of tokens obtained via localStorage.
pub(super) fn leaks_real_token(url: &str, payload: Option<&str>) -> bool {
    with_state(|state| {
        let mut tokens: Vec<&str> = Vec::new();

        // captured_token may be set before token_state (window between jsrt.set_token
        // and the first /oauth2/token response).
        if !state.captured_token.is_empty() {
            tokens.push(state.captured_token.as_str());
        }

        if let Some(ref ts) = state.token_state {
            tokens.push(ts.current.access_token.as_str());
            tokens.push(ts.current.refresh_token.as_str());
            if let Some(ref prev) = ts.previous {
                tokens.push(prev.access_token.as_str());
                tokens.push(prev.refresh_token.as_str());
            }
        }

        for tok in &tokens {
            if tok.is_empty() {
                continue;
            }
            if url.contains(tok) {
                return true;
            }
            if let Some(p) = payload
                && p.contains(tok)
            {
                return true;
            }
        }
        false
    })
    .unwrap_or(false)
}

/// Restrict proxy channels to Tidal domains only.
/// Returns true (and sends an IPC error) if the URL is rejected.
fn reject_non_tidal(url: &str, channel: &str, callback: &IpcCallback) -> bool {
    if !crate::ui::token_filter::should_rewrite_token(url)
        && !crate::ui::nav::is_token_endpoint(url)
    {
        crate::vprintln!(
            "[PROXY]  REJECTED {} to non-Tidal URL: {}",
            channel,
            &url[..url.len().min(80)]
        );
        ipc_callback_err(callback, &format!("{channel}: non-Tidal URL rejected"));
        return true;
    }
    false
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

    if reject_non_tidal(&url, "proxy.fetch", &callback) {
        return;
    }

    with_state(|state| {
        state.pending_ipc_callbacks.insert(id.clone(), callback);
    });
    crate::state::rt_handle().spawn(async move {
        handle_proxy_fetch(id, url, opts_json).await;
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

    if reject_non_tidal(&url, "proxy.head", &callback) {
        return;
    }

    with_state(|state| {
        state.pending_ipc_callbacks.insert(id.clone(), callback);
    });
    crate::state::rt_handle().spawn(async move {
        handle_proxy_head(id, url).await;
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

    let rewrite_auth = crate::ui::token_filter::should_rewrite_token(&url);
    if let Some(headers_str) = opts.get("headers").and_then(|v| v.as_str())
        && let Ok(headers) =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(headers_str)
    {
        for (key, value) in &headers {
            if rewrite_auth
                && key.eq_ignore_ascii_case("authorization")
                && value
                    .as_str()
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .is_some_and(crate::ui::token_filter::is_opaque)
            {
                continue;
            }
            if let Some(val) = value.as_str() {
                req = req.header(key.as_str(), val);
            }
        }
    }

    if let Some(body) = opts.get("body").and_then(|v| v.as_str()) {
        let body = if crate::ui::nav::is_token_endpoint(&url) {
            // Capture client_id from token exchange body (defensive: covers proxy path)
            for (k, v) in url::form_urlencoded::parse(body.as_bytes()) {
                if k == "client_id" && !v.is_empty() {
                    with_state(|state| {
                        state.last_client_id = v.to_string();
                    });
                    break;
                }
            }
            proxy_rewrite_refresh_body(body)
        } else {
            body.to_string()
        };
        req = req.body(body);
    }

    let has_auth = opts
        .get("headers")
        .and_then(|v| v.as_str())
        .and_then(|h| serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(h).ok())
        .map(|map| map.keys().any(|k| k.eq_ignore_ascii_case("authorization")))
        .unwrap_or(false);
    if !has_auth && crate::ui::token_filter::needs_auto_injection(&url) {
        let token = with_state(|state| state.captured_token.clone()).unwrap_or_default();
        if !token.is_empty() {
            req = req.header("Authorization", format!("Bearer {token}"));
            crate::vprintln!(
                "[PROXY]  Injected captured token for {}",
                &url[..url.len().min(80)]
            );
        }
    } else if has_auth
        && crate::ui::token_filter::should_rewrite_token(&url)
        && let Some(auth_val) = opts
            .get("headers")
            .and_then(|v| v.as_str())
            .and_then(|h| {
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(h).ok()
            })
            .and_then(|map| {
                map.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                    .and_then(|(_, v)| v.as_str().map(|s| s.to_string()))
            })
        && let Some(opaque) = auth_val.strip_prefix("Bearer ")
        && crate::ui::token_filter::is_opaque(opaque)
    {
        let real = with_state(|state| {
            let ts = state.token_state.as_ref()?;
            if opaque == ts.current.opaque_at {
                return Some(ts.current.access_token.clone());
            }
            if let Some(ref prev) = ts.previous {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if ts.previous_valid_until.is_none_or(|until| now <= until)
                    && opaque == prev.opaque_at
                {
                    return Some(prev.access_token.clone());
                }
            }
            None
        })
        .flatten();
        if let Some(real_token) = real {
            req = req.header("Authorization", format!("Bearer {real_token}"));
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
            let is_token_endpoint = crate::ui::nav::is_token_endpoint(&url);
            let is_4xx = (400..500).contains(&(status as u32));
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
            let body = if is_token_endpoint {
                proxy_transform_token_body(&body, status)
            } else {
                body
            };
            let json = serde_json::json!({
                "status": status,
                "body": body,
                "headers": headers_map,
            });
            ipc_callback_ok(&callback, &json.to_string());
        }
        Err(e) => {
            ipc_callback_err(&callback, &format!("proxy.fetch failed: {e}"));
        }
    }
}

fn proxy_rewrite_refresh_body(body: &str) -> String {
    let params: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    let is_refresh = params
        .iter()
        .any(|(k, v)| k == "grant_type" && v == "refresh_token");
    if !is_refresh {
        return body.to_string();
    }

    let rt_value = params
        .iter()
        .find(|(k, _)| k == "refresh_token")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");

    if !crate::ui::token_filter::is_opaque(rt_value) {
        return body.to_string();
    }

    let real_rt = with_state(|state| {
        let ts = state.token_state.as_ref()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if rt_value == ts.current.opaque_rt {
            return Some(ts.current.refresh_token.clone());
        }
        if let Some(ref prev) = ts.previous
            && ts.previous_valid_until.is_none_or(|until| now <= until)
            && rt_value == prev.opaque_rt
        {
            return Some(ts.current.refresh_token.clone());
        }
        None
    })
    .flatten();

    let Some(real_rt) = real_rt else {
        return body.to_string();
    };

    params
        .iter()
        .map(|(k, v)| {
            let val = if k == "refresh_token" { &real_rt } else { v };
            format!(
                "{}={}",
                url::form_urlencoded::byte_serialize(k.as_bytes()).collect::<String>(),
                url::form_urlencoded::byte_serialize(val.as_bytes()).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn proxy_transform_token_body(body: &str, status: u16) -> String {
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(body) else {
        crate::vprintln!("[AUTH]   /oauth2/token → {} (non-JSON)", status);
        return body.to_string();
    };
    let Some(obj) = json.as_object_mut() else {
        return body.to_string();
    };

    let access_token = obj
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let Some(at) = access_token else {
        if let Some(err) = obj.get("error").and_then(|v| v.as_str()) {
            let desc = obj
                .get("error_description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            crate::vprintln!(
                "[AUTH]   /oauth2/token → {} error={}: {}",
                status,
                err,
                desc
            );
        }
        return body.to_string();
    };

    let refresh_token = obj
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let expires_in = obj.get("expires_in").and_then(|v| v.as_u64());
    let user_id = obj
        .get("user_id")
        .and_then(|v| v.as_u64())
        .map(|v| v.to_string());
    let scope = obj
        .get("scope")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let opaque_at = crate::ui::token_filter::generate_opaque();
    let granted_scopes: Vec<String> = scope
        .as_deref()
        .map(|s| s.split(' ').map(|s| s.to_string()).collect())
        .unwrap_or_default();

    with_state(|state| {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let (real_rt, ort) = if let Some(ref rt) = refresh_token {
            (rt.clone(), crate::ui::token_filter::generate_opaque())
        } else if let Some(ref ts) = state.token_state {
            (
                ts.current.refresh_token.clone(),
                ts.current.opaque_rt.clone(),
            )
        } else {
            (String::new(), String::new())
        };

        let new_gen = crate::platform::secure_store::TokenGeneration {
            access_token: at.clone(),
            refresh_token: real_rt,
            opaque_at: opaque_at.clone(),
            opaque_rt: ort,
            version: state
                .token_state
                .as_ref()
                .map(|ts| ts.current.version + 1)
                .unwrap_or(1),
            access_expires: now_secs + expires_in.unwrap_or(3600),
            user_id: user_id.clone(),
            granted_scopes: granted_scopes.clone(),
            client_id: state
                .token_state
                .as_ref()
                .map(|ts| ts.current.client_id.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| state.last_client_id.clone()),
        };

        let previous = state.token_state.as_ref().map(|ts| ts.current.clone());
        state.token_state = Some(crate::platform::secure_store::StoredTokenState {
            current: new_gen,
            previous,
            previous_valid_until: Some(now_secs + 30),
        });

        state.captured_token = at.clone();

        let data_dir = crate::state::cache_data_dir();
        if let Some(ref ts) = state.token_state {
            let _ = crate::platform::secure_store::save(&data_dir, ts);
        }
    });

    crate::ipc::plugin::scrub_pkce_verifier();
    crate::vprintln!(
        "[AUTH]   /oauth2/token → {} (captured via proxy, {} chars)",
        status,
        at.len()
    );

    obj.insert(
        "access_token".to_string(),
        serde_json::Value::String(opaque_at),
    );
    if refresh_token.is_some() {
        let ort = with_state(|state| {
            state
                .token_state
                .as_ref()
                .map(|ts| ts.current.opaque_rt.clone())
        })
        .flatten()
        .unwrap_or_default();
        obj.insert("refresh_token".to_string(), serde_json::Value::String(ort));
    }

    serde_json::to_string(&json).unwrap_or_else(|_| body.to_string())
}
