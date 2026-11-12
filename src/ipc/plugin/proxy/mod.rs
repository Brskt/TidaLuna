use super::{ipc_callback_err, ipc_callback_ok, take_ipc_callback};
use crate::app_state::{IpcCallback, IpcMessage, with_state};
use cef::ImplCookieManager;

mod scrub;
mod token_exchange;

pub(super) use scrub::leaks_real_token;
pub(crate) use scrub::scrub_real_tokens;
use scrub::{real_token_pairs, scrub_real_tokens_with};
use token_exchange::{proxy_rewrite_refresh_body, proxy_transform_token_body};

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

/// Cap on an upstream body buffered for a plugin. The target is a plugin's to choose, and
/// `proxy.fetch` is gated to TIDAL hosts whose API answers are JSON; a large asset under one of
/// them is bounded here without cutting into any real response.
const MAX_UPSTREAM_BYTES: usize = 8 * 1024 * 1024;

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

/// Whether a reply this path answers carries artwork we rewrite. The mime lives in the
/// upstream headers here rather than on a CEF response. The reading is the handler's;
/// the rule it feeds is the one the response filter uses.
fn strips_artwork(
    url: &crate::ui::nav::RequestUrl,
    headers: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    let ready = crate::video::codec::is_ready();
    headers
        .get("content-type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|mime| crate::ui::artwork_filter::strips_artwork_body(url, mime, ready))
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
    fn transform_token_body(
        self,
        status: u16,
        session_epoch: u64,
        exchange_client_id: Option<&str>,
    ) -> Self {
        Self(proxy_transform_token_body(
            &self.0,
            status,
            session_epoch,
            exchange_client_id,
        ))
    }

    /// The artwork rewrite runs here as well as on the CEF response filter, because a reply
    /// this path answers never reaches that filter: `proxy.fetch` drives reqwest itself, and
    /// there is no CEF response to attach one to. Both sites call the same rule; leaving it
    /// to the filter alone would hand back the video cover our renderer cannot decode
    /// whenever the renderer's own fetch failed and fell back here.
    fn strip_video_artwork(self) -> Self {
        match crate::ui::artwork_filter::strip_video_artwork(self.0.as_bytes()) {
            // `serde_json` emits UTF-8; keeping the original on the impossible branch is
            // still better than handing back a lossy rewrite of a body we own.
            crate::ui::artwork_filter::StripResult::Rewritten(bytes) => {
                match String::from_utf8(bytes) {
                    Ok(stripped) => Self(stripped),
                    Err(_) => self,
                }
            }
            crate::ui::artwork_filter::StripResult::Unchanged => self,
        }
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

    // The commit pairs any refresh token minted here with this request's client_id.
    let exchange_client_id = opts
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|_| crate::ui::nav::is_token_endpoint(&url))
        .and_then(crate::ui::token_filter::form_client_id);

    if let Some(body) = opts.get("body").and_then(|v| v.as_str()) {
        let body = if crate::ui::nav::is_token_endpoint(&url) {
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
        let now = crate::util::now_unix_secs();
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

    // Read before the round trip: it names the session this fetch answers for, and a token
    // endpoint's reply commits credentials on the way back out.
    let session_epoch = with_state(|state| state.session_epoch).unwrap_or_default();

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
            let body = match super::source::read_bytes_capped(
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
            // Read before `headers_map` is handed to the reply, and settled as a bool so the
            // borrow ends here rather than straddling that move.
            let strip_artwork = strips_artwork(&url, &headers_map);
            if strip_artwork {
                crate::video::codec::note_demand();
            }
            let body = if is_token_endpoint {
                body.transform_token_body(status, session_epoch, exchange_client_id.as_deref())
            } else if strip_artwork {
                body.strip_video_artwork()
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

#[cfg(test)]
#[path = "../../../../tests/unit/ipc/plugin/proxy.rs"]
mod tests;
