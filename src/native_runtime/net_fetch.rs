//! net.fetch RPC: the Bun child emits a net.fetch line on stdout; we HTTP it via
//! reqwest and write net.fetch.result back. Auth is the cjs trust gate. Each plugin
//! gets a cookie-isolated client pair (follow + no-redirect) so cookies can't link
//! plugins; in-flight fetches are tracked by reqId so net.fetch.cancel can drop one.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use base64::Engine;
use serde_json::Value;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

/// Max response body accepted from a sanctioned plugin fetch (cover art etc.).
const MAX_BODY_BYTES: usize = 25 * 1024 * 1024;
/// Per-request timeout.
const REQUEST_TIMEOUT_SECS: u64 = 30;
/// Cap on concurrent in-flight egress fetches (each may buffer up to the body cap).
const MAX_CONCURRENT_FETCHES: usize = 16;

/// How a `net.fetch` follows HTTP redirects (mirrors the WHATWG `redirect` init).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RedirectMode {
    Follow,
    Manual,
    Error,
}

/// A validated `net.fetch` request.
pub(crate) struct ParsedFetch {
    pub req_id: Value,
    pub plugin: String,
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub redirect: RedirectMode,
}

/// Uppercase the method and refuse verbs a browser `fetch` never issues
/// (CONNECT/TRACE/TRACK have no place in plugin egress).
fn normalize_method(raw: &str) -> Result<String, String> {
    let m = raw.trim().to_ascii_uppercase();
    if m.is_empty() {
        return Err("method must not be empty".to_string());
    }
    if matches!(m.as_str(), "CONNECT" | "TRACE" | "TRACK") {
        return Err(format!("method {m} is not allowed"));
    }
    Ok(m)
}

/// Read the `redirect` init (defaults to follow, matching `fetch`).
fn parse_redirect_mode(req: &Value) -> RedirectMode {
    match req.get("redirect").and_then(|v| v.as_str()) {
        Some("manual") => RedirectMode::Manual,
        Some("error") => RedirectMode::Error,
        _ => RedirectMode::Follow,
    }
}

/// Headers arrive as an ordered array of `[name, value]` pairs so duplicate
/// names (e.g. repeated request headers) survive instead of collapsing.
fn parse_headers(req: &Value) -> Vec<(String, String)> {
    req.get("headers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|pair| {
                    let p = pair.as_array()?;
                    Some((
                        p.first()?.as_str()?.to_string(),
                        p.get(1)?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse + validate a `net.fetch` request. On error, returns the request id
/// (so the caller can still send a `net.fetch.result` error) plus a message.
pub(crate) fn parse_fetch_request(req: &Value) -> Result<ParsedFetch, (Value, String)> {
    let req_id = req.get("reqId").cloned().unwrap_or(Value::Null);

    // data:/blob: are resolved in the child; only http(s) ever reaches Rust.
    let url = req
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err((req_id, "url must be http(s)".to_string()));
    }

    let plugin = req
        .get("plugin")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let method = normalize_method(req.get("method").and_then(|v| v.as_str()).unwrap_or("GET"))
        .map_err(|e| (req_id.clone(), e))?;

    let headers = parse_headers(req);

    let body = match req.get("body").and_then(|v| v.as_str()) {
        Some(b64) => {
            // Reject oversized bodies on length (decoded ≈ encoded * 3/4) before the
            // decode allocates - a plugin must not force a giant host buffer, and this
            // runs before the concurrency semaphore in run_fetch.
            if b64.len() / 4 * 3 > MAX_BODY_BYTES {
                return Err((req_id, "request body exceeds limit".to_string()));
            }
            Some(
                base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|_| (req_id.clone(), "invalid base64 body".to_string()))?,
            )
        }
        None => None,
    };

    let redirect = parse_redirect_mode(req);

    Ok(ParsedFetch {
        req_id,
        plugin,
        url,
        method,
        headers,
        body,
        redirect,
    })
}

/// Serialize a successful `net.fetch.result`. Headers are an ordered array of
/// `[name, value]` pairs (preserving duplicate Set-Cookie/Link), plus the final
/// URL and whether a redirect was followed. Body is base64-encoded.
pub(crate) fn build_ok_result(
    req_id: &Value,
    status: u16,
    status_text: &str,
    headers: &[(String, String)],
    final_url: &str,
    redirected: bool,
    body: &[u8],
) -> String {
    let harr: Vec<Value> = headers
        .iter()
        .map(|(k, v)| Value::Array(vec![Value::String(k.clone()), Value::String(v.clone())]))
        .collect();
    let body_b64 = base64::engine::general_purpose::STANDARD.encode(body);
    serde_json::json!({
        "type": "net.fetch.result",
        "reqId": req_id,
        "ok": true,
        "status": status,
        "statusText": status_text,
        "url": final_url,
        "redirected": redirected,
        "headers": Value::Array(harr),
        "body": body_b64,
    })
    .to_string()
}

/// Serialize a failed `net.fetch.result`.
pub(crate) fn build_error_result(req_id: &Value, msg: &str) -> String {
    serde_json::json!({
        "type": "net.fetch.result",
        "reqId": req_id,
        "ok": false,
        "error": msg,
    })
    .to_string()
}

/// A plugin's two cookie-jar-sharing clients: one follows redirects, one does
/// not (for `redirect:'manual'`/`'error'`). Both share the same `Arc<Jar>` so a
/// plugin's cookies persist regardless of the redirect mode of a given request.
struct PluginClients {
    follow: reqwest::Client,
    manual: reqwest::Client,
}

/// Per-plugin clients, keyed by plugin id, so cookies never link two plugins.
static PLUGIN_CLIENTS: LazyLock<Mutex<HashMap<String, PluginClients>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Bound concurrent egress so a plugin firing many fetches can't spawn unbounded
/// 25 MB buffers at once.
static FETCH_SEMAPHORE: LazyLock<Semaphore> =
    LazyLock::new(|| Semaphore::new(MAX_CONCURRENT_FETCHES));

/// In-flight fetches keyed by reqId, so a `net.fetch.cancel` can abort one.
static INFLIGHT: LazyLock<Mutex<HashMap<String, CancellationToken>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Get (or lazily build) the cookie-isolated client for a plugin + redirect mode.
fn client_for(plugin: &str, redirect: RedirectMode) -> reqwest::Client {
    let mut map = PLUGIN_CLIENTS.lock().unwrap_or_else(|e| e.into_inner());
    let entry = map.entry(plugin.to_string()).or_insert_with(|| {
        let jar = Arc::new(reqwest::cookie::Jar::default());
        PluginClients {
            follow: crate::state::build_native_client(
                jar.clone(),
                reqwest::redirect::Policy::default(),
            ),
            manual: crate::state::build_native_client(jar, reqwest::redirect::Policy::none()),
        }
    });
    match redirect {
        RedirectMode::Follow => entry.follow.clone(),
        RedirectMode::Manual | RedirectMode::Error => entry.manual.clone(),
    }
}

/// Map a request's `reqId` to a stable cancellation key (None if absent/null).
fn reqid_key(req: &Value) -> Option<String> {
    match req.get("reqId") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.to_string()),
    }
}

fn unregister(key: &Option<String>) {
    if let Some(k) = key {
        INFLIGHT.lock().unwrap_or_else(|e| e.into_inner()).remove(k);
    }
}

/// Spawn a `net.fetch` on the runtime, tracked so a `net.fetch.cancel` can abort it.
pub(crate) fn dispatch(req: Value, stdin_tx: mpsc::UnboundedSender<String>) {
    let key = reqid_key(&req);
    let token = CancellationToken::new();
    if let Some(ref k) = key {
        INFLIGHT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(k.clone(), token.clone());
    }
    crate::state::rt_handle().spawn(handle_net_fetch(req, stdin_tx, token, key));
}

/// Cancel an in-flight `net.fetch` (the child aborted via AbortSignal).
pub(crate) fn cancel(req: &Value) {
    if let Some(k) = reqid_key(req)
        && let Some(token) = INFLIGHT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&k)
    {
        token.cancel();
    }
}

/// Run one `net.fetch`, sending its result on `stdin_tx`, unless cancelled first.
async fn handle_net_fetch(
    req: Value,
    stdin_tx: mpsc::UnboundedSender<String>,
    token: CancellationToken,
    key: Option<String>,
) {
    tokio::select! {
        biased;
        // The child already rejected the plugin's promise; drop silently.
        _ = token.cancelled() => {}
        _ = run_fetch(req, &stdin_tx) => {}
    }
    unregister(&key);
}

/// Execute the HTTP round-trip and send a result. Errors become `ok:false`
/// results, never panics.
async fn run_fetch(req: Value, stdin_tx: &mpsc::UnboundedSender<String>) {
    let parsed = match parse_fetch_request(&req) {
        Ok(p) => p,
        Err((req_id, msg)) => {
            let _ = stdin_tx.send(build_error_result(&req_id, &msg));
            return;
        }
    };

    // normalize_method already validated the token, so from_bytes can't fail.
    let method = match reqwest::Method::from_bytes(parsed.method.as_bytes()) {
        Ok(m) => m,
        Err(_) => {
            let _ = stdin_tx.send(build_error_result(&parsed.req_id, "invalid method"));
            return;
        }
    };

    let Ok(_permit) = FETCH_SEMAPHORE.acquire().await else {
        return;
    };

    let client = client_for(&parsed.plugin, parsed.redirect);
    let mut builder = client
        .request(method, &parsed.url)
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS));
    for (k, v) in &parsed.headers {
        builder = builder.header(k, v);
    }
    if let Some(body) = parsed.body {
        builder = builder.body(body);
    }

    let mut resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
            // reqwest's error Display appends the full request URL (query + fragment),
            // which would bypass the redaction above; strip it before logging/forwarding.
            let e = e.without_url();
            crate::vprintln!(
                "[NATIVE:FETCH] {} {} {} ERR {}",
                parsed.plugin,
                parsed.method,
                crate::util::redact_url_query(&parsed.url),
                e
            );
            let _ = stdin_tx.send(build_error_result(&parsed.req_id, &e.to_string()));
            return;
        }
    };

    let status = resp.status().as_u16();
    // redirect:'error' must reject any 3xx (the manual client doesn't follow).
    if parsed.redirect == RedirectMode::Error && (300..400).contains(&status) {
        let _ = stdin_tx.send(build_error_result(&parsed.req_id, "redirect not allowed"));
        return;
    }
    let status_text = resp.status().canonical_reason().unwrap_or("").to_string();
    let final_url = resp.url().to_string();
    let redirected = final_url != parsed.url;

    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in resp.headers() {
        if let Ok(s) = value.to_str() {
            headers.push((name.as_str().to_string(), s.to_string()));
        }
    }

    // Stream the body with a hard cap (fail-closed on oversize).
    let mut body: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
                    let _ = stdin_tx.send(build_error_result(
                        &parsed.req_id,
                        "response body exceeds limit",
                    ));
                    return;
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => {
                let _ = stdin_tx.send(build_error_result(&parsed.req_id, &e.to_string()));
                return;
            }
        }
    }

    // Defence-in-depth: scrub a leaked real token from a text body before the
    // plugin sees it. A binary body (cover art) can't hold a UTF-8 token.
    let scrubbed = std::str::from_utf8(&body)
        .ok()
        .map(|t| crate::ipc::plugin::scrub_real_tokens(t.to_string()));
    if let Some(s) = scrubbed
        && s.as_bytes() != body.as_slice()
    {
        body = s.into_bytes();
    }

    crate::vprintln!(
        "[NATIVE:FETCH] {} {} {} {} ({} bytes)",
        parsed.plugin,
        parsed.method,
        crate::util::redact_url_query(&parsed.url),
        status,
        body.len()
    );

    let _ = stdin_tx.send(build_ok_result(
        &parsed.req_id,
        status,
        &status_text,
        &headers,
        &final_url,
        redirected,
        &body,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_rejects_non_http_scheme() {
        // data:/blob: are resolved in the child; only http(s) reaches Rust.
        let req = json!({ "reqId": 1, "plugin": "P", "url": "file:///etc/passwd" });
        assert!(parse_fetch_request(&req).is_err());
    }

    #[test]
    fn parse_rejects_missing_url() {
        let req = json!({ "reqId": 1, "plugin": "P" });
        assert!(parse_fetch_request(&req).is_err());
    }

    #[test]
    fn parse_defaults_method_get() {
        let req = json!({ "reqId": 7, "plugin": "CoverTheme", "url": "https://h/p" });
        let p = parse_fetch_request(&req).expect("valid");
        assert_eq!(p.method, "GET");
        assert_eq!(p.plugin, "CoverTheme");
        assert!(p.body.is_none());
    }

    #[test]
    fn parse_uppercases_method() {
        let req = json!({ "reqId": 1, "plugin": "P", "url": "https://h/p", "method": "post" });
        assert_eq!(parse_fetch_request(&req).expect("valid").method, "POST");
    }

    #[test]
    fn parse_rejects_connect_and_trace_methods() {
        for m in ["CONNECT", "connect", "TRACE", "trace", "TRACK"] {
            let req = json!({ "reqId": 1, "plugin": "P", "url": "https://h/p", "method": m });
            assert!(
                parse_fetch_request(&req).is_err(),
                "method {m} must be rejected"
            );
        }
    }

    #[test]
    fn parse_headers_array_preserves_duplicates_and_order() {
        let req = json!({
            "reqId": 1, "plugin": "P", "url": "https://h/p",
            "headers": [["accept", "a"], ["accept", "b"], ["x-test", "1"]]
        });
        let p = parse_fetch_request(&req).expect("valid");
        assert_eq!(
            p.headers,
            vec![
                ("accept".to_string(), "a".to_string()),
                ("accept".to_string(), "b".to_string()),
                ("x-test".to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn parse_redirect_mode_defaults_follow_and_reads_values() {
        let base = json!({ "reqId": 1, "plugin": "P", "url": "https://h/p" });
        assert!(matches!(
            parse_fetch_request(&base).unwrap().redirect,
            RedirectMode::Follow
        ));
        let manual =
            json!({ "reqId": 1, "plugin": "P", "url": "https://h/p", "redirect": "manual" });
        assert!(matches!(
            parse_fetch_request(&manual).unwrap().redirect,
            RedirectMode::Manual
        ));
        let err = json!({ "reqId": 1, "plugin": "P", "url": "https://h/p", "redirect": "error" });
        assert!(matches!(
            parse_fetch_request(&err).unwrap().redirect,
            RedirectMode::Error
        ));
    }

    #[test]
    fn parse_decodes_base64_body() {
        // base64("hello") = aGVsbG8=
        let req = json!({
            "reqId": 1, "plugin": "P", "url": "https://h/p",
            "method": "POST", "body": "aGVsbG8="
        });
        let p = parse_fetch_request(&req).expect("valid");
        assert_eq!(p.method, "POST");
        assert_eq!(p.body.as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn parse_rejects_bad_base64_body() {
        let req = json!({
            "reqId": 1, "plugin": "P", "url": "https://h/p", "body": "!!!not base64!!!"
        });
        assert!(parse_fetch_request(&req).is_err());
    }

    #[test]
    fn parse_rejects_oversized_request_body_before_decoding() {
        // A base64 string whose decoded size would exceed the cap is rejected on
        // its length (cheap), before any large decode allocation.
        let oversized = "A".repeat(MAX_BODY_BYTES / 3 * 4 + 8);
        let req = json!({
            "reqId": 1, "plugin": "P", "url": "https://h/p", "method": "POST", "body": oversized
        });
        assert!(parse_fetch_request(&req).is_err());
    }

    #[test]
    fn build_error_result_shape() {
        let line = build_error_result(&json!(9), "boom");
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["type"], "net.fetch.result");
        assert_eq!(v["reqId"], json!(9));
        assert_eq!(v["ok"], json!(false));
        assert_eq!(v["error"], "boom");
    }

    #[test]
    fn build_ok_result_carries_url_redirected_and_dup_headers() {
        // Duplicate Set-Cookie must survive as an ordered array of [k,v] pairs.
        let headers = vec![
            ("set-cookie".to_string(), "a=1".to_string()),
            ("set-cookie".to_string(), "b=2".to_string()),
            ("content-type".to_string(), "image/jpeg".to_string()),
        ];
        let line = build_ok_result(
            &json!(3),
            200,
            "OK",
            &headers,
            "https://h/final",
            true,
            b"\x00\x01hi",
        );
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["status"], json!(200));
        assert_eq!(v["statusText"], "OK");
        assert_eq!(v["url"], "https://h/final");
        assert_eq!(v["redirected"], json!(true));
        let h = v["headers"].as_array().expect("headers array");
        assert_eq!(h.len(), 3);
        assert_eq!(h[0], json!(["set-cookie", "a=1"]));
        assert_eq!(h[1], json!(["set-cookie", "b=2"]));
        assert_eq!(h[2], json!(["content-type", "image/jpeg"]));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(v["body"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, b"\x00\x01hi");
    }
}
