use super::{ipc_callback_err, ipc_callback_ok, take_ipc_callback};
use crate::app_state::{AppState, IpcCallback, IpcMessage, with_state};
use cef::ImplCookieManager;

fn parse_set_cookie(header: &str) -> Option<cef::Cookie> {
    // Format: "name=value; Path=/; Domain=.tidal.com; Secure; HttpOnly; ..."
    let mut parts = header.splitn(2, ';');
    let (name, value) = parts.next()?.split_once('=')?;
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() {
        return None;
    }

    let mut domain = String::new();
    let mut path = String::from("/");
    let mut secure = false;
    let mut httponly = false;

    if let Some(attrs) = parts.next() {
        for attr in attrs.split(';') {
            let attr = attr.trim();
            if let Some((k, v)) = attr.split_once('=') {
                let k = k.trim();
                if k.eq_ignore_ascii_case("domain") {
                    domain = v.trim().to_string();
                } else if k.eq_ignore_ascii_case("path") {
                    path = v.trim().to_string();
                }
            } else if attr.eq_ignore_ascii_case("secure") {
                secure = true;
            } else if attr.eq_ignore_ascii_case("httponly") {
                httponly = true;
            }
        }
    }

    Some(cef::Cookie {
        size: std::mem::size_of::<cef::Cookie>(),
        name: cef::CefString::from(name),
        value: cef::CefString::from(value),
        domain: cef::CefString::from(domain.as_str()),
        path: cef::CefString::from(path.as_str()),
        secure: secure.into(),
        httponly: httponly.into(),
        creation: cef::Basetime { val: 0 },
        last_access: cef::Basetime { val: 0 },
        has_expires: 0,
        expires: cef::Basetime { val: 0 },
        same_site: Default::default(),
        priority: Default::default(),
    })
}

/// Replacement for a real token with no opaque mapping (a captured token seen before
/// token_state exists). Must not start with `luna_`: `is_opaque()` would otherwise accept it.
const REDACTED_MARKER: &str = "[redacted-token]";

/// Cap on an upstream body buffered for a plugin. The target is a plugin's to choose, and
/// `proxy.fetch` is gated to TIDAL hosts whose API answers are JSON; a large asset under one of
/// them is bounded here without cutting into any real response.
const MAX_UPSTREAM_BYTES: usize = 8 * 1024 * 1024;

/// Real OAuth tokens in play paired with the opaque they map to. Single source for
/// `scrub_real_tokens` (uses both halves) and `leaks_real_token` (uses the real).
fn real_token_pairs(state: &AppState) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    if let Some(ref ts) = state.token_state {
        pairs.push((
            ts.current.access_token.clone(),
            ts.current.opaque_at.clone(),
        ));
        pairs.push((
            ts.current.refresh_token.clone(),
            ts.current.opaque_rt.clone(),
        ));
        if let Some(ref prev) = ts.previous {
            pairs.push((prev.access_token.clone(), prev.opaque_at.clone()));
            pairs.push((prev.refresh_token.clone(), prev.opaque_rt.clone()));
        }
    }
    // captured_token can precede token_state; add it only if it isn't already the
    // current access token (dedup), with no opaque to map to.
    let cap = &state.captured_token;
    if !cap.is_empty()
        && state
            .token_state
            .as_ref()
            .is_none_or(|ts| ts.current.access_token != *cap)
    {
        pairs.push((cap.clone(), REDACTED_MARKER.to_string()));
    }
    pairs
}

/// True if `url`/`payload` contains any real OAuth token. Defence-in-depth against
/// naive exfiltration of tokens obtained via localStorage.
pub(super) fn leaks_real_token(url: &str, payload: Option<&str>) -> bool {
    with_state(|state| {
        real_token_pairs(state).iter().any(|(real, _)| {
            !real.is_empty()
                && (url.contains(real.as_str())
                    || payload.is_some_and(|p| p.contains(real.as_str())))
        })
    })
    .unwrap_or(false)
}

/// Restrict proxy channels to Tidal domains only.
/// Returns true (and sends an IPC error) if the URL is rejected.
fn reject_non_tidal(
    url: &crate::ui::nav::RequestUrl,
    channel: &str,
    callback: &IpcCallback,
) -> bool {
    if !crate::ui::nav::is_tidal_origin(url) && !crate::ui::nav::is_token_endpoint(url) {
        crate::vprintln!(
            "[PROXY]  REJECTED {} to non-Tidal URL: {}",
            channel,
            crate::util::truncate_str(&crate::util::redact_url_query(url.as_str()), 80)
        );
        ipc_callback_err(callback, 403, &format!("{channel}: non-Tidal URL rejected"));
        return true;
    }
    false
}

pub(super) fn handle_proxy_fetch_dispatch(msg: &IpcMessage, query_id: i64, callback: IpcCallback) {
    let url = crate::ui::nav::RequestUrl::new(
        msg.args
            .first()
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    );
    let opts_json = msg
        .args
        .get(1)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if reject_non_tidal(&url, "proxy.fetch", &callback) {
        return;
    }

    with_state(|state| {
        state.pending_ipc_callbacks.insert(query_id, callback);
    });
    crate::state::rt_handle().spawn(async move {
        handle_proxy_fetch(query_id, url, opts_json).await;
    });
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

/// A response body as it came off the wire. An upstream error page can reflect a real OAuth
/// token back at us, hence no accessor for the raw string: `scrubbed_for_log` and
/// `into_reply` are the only exits and both run the token scrub.
struct UpstreamBody(String);

impl UpstreamBody {
    /// A scrubbed prefix of at most `max_bytes`.
    ///
    /// Scrub the whole body first, truncate second, never the reverse. A cut ahead of the
    /// scrub hands it a fragment it cannot match, and each substitution shortens the string,
    /// sliding that fragment into the window returned here; widening the cut only covers the
    /// token straddling it, not the one the shrinkage carries into view.
    ///
    /// Callers gate the call themselves, `format_args!` evaluating its arguments even at a
    /// level that discards the line.
    fn scrubbed_for_log(&self, max_bytes: usize) -> String {
        let pairs = with_state(|state| real_token_pairs(state)).unwrap_or_default();
        self.scrubbed_for_log_with(max_bytes, &pairs)
    }

    /// The seam behind [`scrubbed_for_log`](Self::scrubbed_for_log). One `APP_STATE` take for
    /// the pairs, and a boundary a test can drive without the global state.
    fn scrubbed_for_log_with(&self, max_bytes: usize, pairs: &[(String, String)]) -> String {
        let scrubbed = scrub_real_tokens_with(self.0.clone(), pairs);
        crate::util::truncate_str(&scrubbed, max_bytes).to_string()
    }

    /// The token-endpoint rewrite is the one caller that must see the real body: it
    /// rewrites the token fields themselves. Its output is re-wrapped, keeping egress gated.
    fn transform_token_body(self, status: u16) -> Self {
        Self(proxy_transform_token_body(&self.0, status))
    }

    /// Serializes the whole reply and scrubs it as one string. Headers can carry a token
    /// too, which is why the gate is wider than the body alone.
    fn into_reply(
        self,
        status: u16,
        headers: serde_json::Map<String, serde_json::Value>,
    ) -> String {
        let json = serde_json::json!({
            "status": status,
            "body": self.0,
            "headers": headers,
        });
        scrub_real_tokens(json.to_string())
    }
}

async fn handle_proxy_fetch(query_id: i64, url: crate::ui::nav::RequestUrl, opts_json: String) {
    let client = &*crate::state::HTTP_CLIENT;

    let opts: serde_json::Map<String, serde_json::Value> = if !opts_json.is_empty() {
        serde_json::from_str(&opts_json).unwrap_or_default()
    } else {
        Default::default()
    };

    let method = opts.get("method").and_then(|v| v.as_str()).unwrap_or("GET");

    let mut req = match method {
        "POST" => client.post(url.as_str()),
        "PUT" => client.put(url.as_str()),
        "PATCH" => client.patch(url.as_str()),
        "DELETE" => client.delete(url.as_str()),
        "HEAD" => client.head(url.as_str()),
        _ => client.get(url.as_str()),
    };

    let rewrite_auth = crate::ui::token_filter::should_rewrite_token(&url);
    let headers_map: Option<serde_json::Map<String, serde_json::Value>> = opts
        .get("headers")
        .and_then(|v| v.as_str())
        .and_then(|h| serde_json::from_str(h).ok());
    if let Some(headers) = &headers_map {
        for (key, value) in headers {
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

    let has_auth = headers_map
        .as_ref()
        .is_some_and(|map| map.keys().any(|k| k.eq_ignore_ascii_case("authorization")));
    if !has_auth && crate::ui::token_filter::needs_auto_injection(&url) {
        let token = with_state(|state| state.captured_token.clone()).unwrap_or_default();
        if !token.is_empty() {
            req = req.header("Authorization", format!("Bearer {token}"));
            crate::vprintln!(
                "[PROXY]  Injected captured token for {}",
                crate::util::truncate_str(&crate::util::redact_url_query(url.as_str()), 80)
            );
        }
    } else if has_auth
        && rewrite_auth
        && let Some(auth_val) = headers_map.as_ref().and_then(|map| {
            map.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .and_then(|(_, v)| v.as_str())
        })
        && let Some(opaque) = auth_val.strip_prefix("Bearer ")
        && crate::ui::token_filter::is_opaque(opaque)
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let real = with_state(|state| {
            state
                .token_state
                .as_ref()
                .map(|ts| crate::ui::token_filter::resolve_opaque_access(ts, opaque, now))
        })
        .flatten();
        if let Some(real_token) = real {
            req = req.header("Authorization", format!("Bearer {real_token}"));
        }
    }

    let result = req.send().await;

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
            // Mirror Set-Cookie to CEF's cookie jar (JS can't - forbidden header).
            if !set_cookies.is_empty() {
                crate::vprintln!(
                    "[PROXY]  Mirroring {} Set-Cookie header(s) for {}",
                    set_cookies.len(),
                    crate::util::truncate_str(url.as_str(), 80)
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
            let body = match super::plugin_ipc::read_bytes_capped(
                resp,
                MAX_UPSTREAM_BYTES,
                "the upstream body",
            )
            .await
            {
                // Lossy, as `Response::text` is. An undecodable byte must not start failing a
                // path that never failed on one.
                Ok(bytes) => UpstreamBody(String::from_utf8_lossy(&bytes).into_owned()),
                // A body that hit the cap or died mid-stream is not a reply; handing back an
                // empty one would tell the plugin the server answered with nothing.
                Err(e) => {
                    let Some(callback) = take_ipc_callback(query_id) else {
                        return;
                    };
                    ipc_callback_err(&callback, 500, &format!("proxy.fetch failed: {e}"));
                    return;
                }
            };
            // By hand because `scrubbed_for_log` scrubs the whole body; as a bare `vprintln!`
            // argument it would run at level zero too.
            if is_4xx && crate::logging::log_level() >= 1 {
                crate::vprintln!(
                    "[PROXY]  {} {} auth={} body={}",
                    status,
                    crate::util::truncate_str(&crate::util::redact_url_query(url.as_str()), 200),
                    has_auth,
                    body.scrubbed_for_log(400)
                );
            }
            let body = if is_token_endpoint {
                body.transform_token_body(status)
            } else {
                body
            };
            // Claimed only now that the body is in hand: the read above streams, and a
            // cancellation during it must find the entry still in the map for
            // `on_query_canceled` to drop. The Set-Cookie mirroring above stays on the
            // earlier side deliberately, being a cookie-jar side effect rather than a reply.
            let Some(callback) = take_ipc_callback(query_id) else {
                return;
            };
            ipc_callback_ok(&callback, &body.into_reply(status, headers_map));
        }
        Err(e) => {
            let Some(callback) = take_ipc_callback(query_id) else {
                return;
            };
            ipc_callback_err(&callback, 500, &format!("proxy.fetch failed: {e}"));
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

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let real_rt = with_state(|state| {
        state
            .token_state
            .as_ref()
            .map(|ts| crate::ui::token_filter::resolve_opaque_refresh(ts, rt_value, now))
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
    proxy_transform_token_body_with(body, status, crate::ui::token_filter::generate_opaque)
}

/// `opaque` is the opaque-nonce generator, injected for testing. When it fails
/// (RNG unavailable) with a real token present, return an empty token body
/// rather than the real token (the recipient is plugin JS).
fn proxy_transform_token_body_with(
    body: &str,
    status: u16,
    opaque: impl Fn() -> Option<String>,
) -> String {
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(body) else {
        crate::vprintln!("[AUTH]   /oauth2/token -> {} (non-JSON)", status);
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
                "[AUTH]   /oauth2/token -> {} error={}: {}",
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

    // A real token is present: if opaque generation fails, return an empty token
    // body rather than the real token (the recipient is plugin JS).
    let opaque_at = match opaque() {
        Some(o) => o,
        None => return "{}".to_string(),
    };
    let opaque_rt_new = if refresh_token.is_some() {
        match opaque() {
            Some(o) => Some(o),
            None => return "{}".to_string(),
        }
    } else {
        None
    };
    let granted_scopes: Vec<String> = scope
        .as_deref()
        .map(|s| s.split(' ').map(|s| s.to_string()).collect())
        .unwrap_or_default();

    let stored_opaque_rt = with_state(|state| {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let (real_rt, ort) = if let Some(ref rt) = refresh_token {
            (rt.clone(), opaque_rt_new.clone().unwrap_or_default())
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
            opaque_rt: ort.clone(),
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
            // Same rule as the response-filter path: a generation with no refresh
            // token cannot re-establish a session and must not replace one on
            // disk that can. Both sites share this AppState and both apply it.
            if ts.current.is_durable() {
                crate::platform::secure_store::save_queued(&data_dir, ts);
            } else {
                crate::vprintln!(
                    "[AUTH]   Exchange carried no refresh token - stored credential kept"
                );
            }
        }

        // Carry opaque_rt out under this lock: it stays paired with opaque_at
        // (a concurrent refresh can't swap token_state in the gap).
        ort
    })
    .unwrap_or_default();

    crate::ipc::plugin::scrub_pkce_verifier();
    crate::vprintln!(
        "[AUTH]   /oauth2/token -> {} (captured via proxy, {} chars)",
        status,
        at.len()
    );

    obj.insert(
        "access_token".to_string(),
        serde_json::Value::String(opaque_at),
    );
    if refresh_token.is_some() {
        obj.insert(
            "refresh_token".to_string(),
            serde_json::Value::String(stored_opaque_rt),
        );
    }

    serde_json::to_string(&json).unwrap_or_else(|_| "{}".to_string())
}

/// Replace any real OAuth token present in `text` with its opaque counterpart.
/// `pairs` is (real_token, replacement), gathered from the token state.
fn scrub_real_tokens_with(text: String, pairs: &[(String, String)]) -> String {
    let mut out = text;
    for (real, replacement) in pairs {
        // Never substring-match a trivially short value (avoids corrupting a body
        // that merely happens to contain a few common characters).
        if real.len() < 8 {
            continue;
        }
        if out.contains(real.as_str()) {
            crate::vprintln!("[PLUGIN:FETCH] scrubbed a real token from a plugin-facing response");
            out = out.replace(real.as_str(), replacement);
        }
    }
    out
}

/// Defence-in-depth: replace any real OAuth token that leaked into a plugin-facing
/// response with its opaque nonce. TIDAL APIs don't echo the bearer: this is normally a no-op.
pub(crate) fn scrub_real_tokens(text: String) -> String {
    let pairs = with_state(|state| real_token_pairs(state)).unwrap_or_default();
    scrub_real_tokens_with(text, &pairs)
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/proxy.rs"]
mod tests;
