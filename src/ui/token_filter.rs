use cef::*;
use std::sync::{Arc, Mutex};

use crate::ui::buffering_filter::{FilterOutcome, new_buffering_filter};

/// Convert a CefStringUserfree to String without the crate's eprintln on null.
pub(crate) fn userfree_to_string(userfree: &CefStringUserfreeUtf16) -> String {
    let raw: Option<&cef::sys::_cef_string_utf16_t> = userfree.into();
    if raw.is_none() {
        return String::new();
    }
    format!("{}", CefString::from(userfree))
}

const OPAQUE_PREFIX: &str = "luna_";

/// Hosts where a missing Authorization header should be auto-filled with the bearer token.
/// Subset of should_rewrite_token - excludes telemetry/DRM hosts that don't need OAuth.
pub(crate) fn needs_auto_injection(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }

    crate::ui::nav::is_tidal_api_host(parsed.host_str().unwrap_or(""))
}

pub(crate) fn should_rewrite_token(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    let host = parsed.host_str().unwrap_or("");
    crate::ui::nav::is_tidal_api_host(host)
        || matches!(host, "login.tidal.com" | "auth.tidal.com")
        || (host == "fp.fa.tidal.com" && parsed.path().starts_with("/license"))
        || (host.starts_with("event-collector.") && host.ends_with(".tidalhi.fi"))
}

/// Opaque `luna_*` token nonce, or `None` if the system RNG is unavailable.
/// Callers generate this off the AppState lock and fail closed on `None`.
pub(crate) fn generate_opaque() -> Option<String> {
    generate_opaque_with(|buf| match getrandom::fill(buf) {
        Ok(()) => true,
        Err(e) => {
            crate::vprintln!("[token_filter] opaque entropy failure: {e}");
            false
        }
    })
}

/// `fill` returns true on success; entropy source is injected for testing.
fn generate_opaque_with(fill: impl FnOnce(&mut [u8]) -> bool) -> Option<String> {
    use std::fmt::Write;
    let mut buf = [0u8; 16];
    if !fill(&mut buf) {
        return None;
    }
    let mut out = String::with_capacity(OPAQUE_PREFIX.len() + buf.len() * 2);
    out.push_str(OPAQUE_PREFIX);
    for b in buf {
        let _ = write!(out, "{b:02x}");
    }
    Some(out)
}

pub(crate) fn is_opaque(value: &str) -> bool {
    value.starts_with(OPAQUE_PREFIX)
}

// Unconditionally cancels the request. Used by the exfiltration guard
// to block sendBeacon (RT_PING) to non-Tidal domains.
wrap_resource_request_handler! {
    pub(super) struct ExfilBlockHandler;

    impl ResourceRequestHandler {
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            ReturnValue::CANCEL
        }
    }
}

wrap_resource_request_handler! {
    pub(super) struct TokenResourceHandler {
        // Per-request slot: the client_id from this exchange's POST body, set in
        // on_before_resource_load and read at response time, so the persisted
        // client_id is bound to the same exchange that minted the refresh_token
        // (the SDK binds each token to its context's client_id). Arc so the
        // macro's per-field Clone shares one slot across both callbacks.
        exchange_client_id: Arc<Mutex<Option<String>>>,
    }

    impl ResourceRequestHandler {
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            if let Some(req) = request {
                let url_cef = req.url();
                let url = userfree_to_string(&url_cef);
                if url.is_empty() {
                    return ReturnValue::CONTINUE;
                }

                if crate::ui::nav::is_token_endpoint(&url) {
                    let accept_name = CefString::from("Accept-Encoding");
                    let accept_val = CefString::from("identity");
                    req.set_header_by_name(Some(&accept_name), Some(&accept_val), 1);
                    capture_client_id(req, &self.exchange_client_id);
                    inject_refresh_token(req, &url);
                }

                if should_rewrite_token(&url) {
                    rewrite_authorization_header(req);
                }
            }
            ReturnValue::CONTINUE
        }

        fn resource_response_filter(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _response: Option<&mut Response>,
        ) -> Option<ResponseFilter> {
            let url_cef = request?.url();
            let url = userfree_to_string(&url_cef);
            if crate::ui::nav::is_token_endpoint(&url) {
                let exchange_cid = self
                    .exchange_client_id
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                Some(new_buffering_filter(
                    0,
                    Arc::new(move |body| {
                        match process_token_response(&body, exchange_cid.as_deref()) {
                            ProcessResult::Modified(v) => FilterOutcome::Emit(v),
                            ProcessResult::Passthrough => FilterOutcome::Emit(body),
                            ProcessResult::Error => FilterOutcome::Drop,
                        }
                    }),
                ))
            } else {
                None
            }
        }
    }
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_post_body(req: &mut Request) -> Option<Vec<u8>> {
    let method_cef = req.method();
    let method = userfree_to_string(&method_cef);
    if method != "POST" {
        return None;
    }
    let post_data = req.post_data()?;
    let count = post_data.element_count();
    if count == 0 {
        return None;
    }
    let mut elements: Vec<Option<PostDataElement>> = vec![None; count];
    post_data.elements(Some(&mut elements));
    let total: usize = elements.iter().flatten().map(|el| el.bytes_count()).sum();
    if total == 0 {
        return None;
    }
    let mut body = Vec::with_capacity(total);
    for el in elements.into_iter().flatten() {
        let n = el.bytes_count();
        if n == 0 {
            continue;
        }
        let mut buf = vec![0u8; n];
        el.bytes(n, buf.as_mut_ptr());
        body.extend_from_slice(&buf);
    }
    if body.is_empty() { None } else { Some(body) }
}

fn capture_client_id(req: &mut Request, exchange_slot: &Mutex<Option<String>>) {
    let Some(body_bytes) = read_post_body(req) else {
        return;
    };
    let Ok(body_str) = std::str::from_utf8(&body_bytes) else {
        return;
    };
    for (k, v) in url::form_urlencoded::parse(body_str.as_bytes()) {
        if k == "client_id" && !v.is_empty() {
            let cid = v.into_owned();
            // Bind this exchange's client_id to its own response (per-request
            // slot). The global stays as a fallback for callers without that
            // slot: the proactive-refresh and proxy paths.
            *exchange_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(cid.clone());
            crate::app_state::with_state(|state| {
                state.last_client_id = cid;
            });
            return;
        }
    }
}

/// Resolve an opaque `luna_*` access nonce to the real access token for the
/// wire. An in-window `previous` match returns that generation's token (the
/// brief rotation overlap); ANY other `luna_*` falls back to `current` - a
/// nonce only ever means "the current user's token", and letting the raw
/// opaque leave would be rejected by TIDAL and leak the placeholder. The caller
/// must have checked `is_opaque`; callers gate this on `token_state` = Some, so
/// a stray opaque during a logged-out window resolves to nothing.
pub(crate) fn resolve_opaque_access(
    ts: &crate::platform::secure_store::StoredTokenState,
    opaque: &str,
    now: u64,
) -> String {
    if let Some(prev) = ts.previous.as_ref()
        && opaque == prev.opaque_at
        && ts.previous_valid_until.is_none_or(|until| now <= until)
    {
        return prev.access_token.clone();
    }
    ts.current.access_token.clone()
}

/// Refresh-token counterpart of [`resolve_opaque_access`]. Keeps the refresh
/// side self-healing: a `luna_*` the SDK persisted past our two-entry window
/// still maps to the current real refresh token instead of leaving raw - which
/// is what let TIDAL reject the SDK's ~1h refresh and log the user out.
pub(crate) fn resolve_opaque_refresh(
    ts: &crate::platform::secure_store::StoredTokenState,
    opaque: &str,
    now: u64,
) -> String {
    if let Some(prev) = ts.previous.as_ref()
        && opaque == prev.opaque_rt
        && ts.previous_valid_until.is_none_or(|until| now <= until)
    {
        return prev.refresh_token.clone();
    }
    ts.current.refresh_token.clone()
}

fn rewrite_authorization_header(req: &mut Request) {
    let auth_name = CefString::from("Authorization");
    let auth_val = req.header_by_name(Some(&auth_name));
    let auth_str = userfree_to_string(&auth_val);
    if !auth_str.starts_with("Bearer ") {
        return;
    }
    let opaque = &auth_str["Bearer ".len()..];
    if !is_opaque(opaque) {
        return;
    }

    // None only when logged out (no token_state); otherwise every luna_*
    // resolves to a real token, so nothing opaque reaches the wire.
    let real_token = crate::app_state::with_state(|state| {
        state
            .token_state
            .as_ref()
            .map(|ts| resolve_opaque_access(ts, opaque, now_unix_secs()))
    })
    .flatten();

    if let Some(token) = real_token {
        let new_val = CefString::from(format!("Bearer {token}").as_str());
        req.set_header_by_name(Some(&auth_name), Some(&new_val), 1);
    }
}

fn inject_refresh_token(req: &mut Request, url: &str) {
    if !crate::ui::nav::is_token_endpoint(url) {
        return;
    }
    let Some(body_bytes) = read_post_body(req) else {
        return;
    };
    let Ok(body_str) = std::str::from_utf8(&body_bytes) else {
        return;
    };
    let params: Vec<(String, String)> = url::form_urlencoded::parse(body_str.as_bytes())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    let is_refresh = params
        .iter()
        .any(|(k, v)| k == "grant_type" && v == "refresh_token");
    if !is_refresh {
        return;
    }

    let Some(rt_value) = params
        .iter()
        .find(|(k, _)| k == "refresh_token")
        .map(|(_, v)| v.as_str())
    else {
        return;
    };

    if !is_opaque(rt_value) {
        return;
    }

    // None only when logged out (no token_state); otherwise every luna_*
    // resolves to the current real refresh token, so the SDK's own refresh
    // never leaves with a raw opaque (the ~1h logout).
    let real_rt = crate::app_state::with_state(|state| {
        state
            .token_state
            .as_ref()
            .map(|ts| resolve_opaque_refresh(ts, rt_value, now_unix_secs()))
    })
    .flatten();

    let Some(real_rt) = real_rt else { return };

    let new_body: String = params
        .iter()
        .map(|(k, v)| {
            if k == "refresh_token" {
                format!(
                    "{}={}",
                    url::form_urlencoded::byte_serialize(k.as_bytes()).collect::<String>(),
                    url::form_urlencoded::byte_serialize(real_rt.as_bytes()).collect::<String>()
                )
            } else {
                format!(
                    "{}={}",
                    url::form_urlencoded::byte_serialize(k.as_bytes()).collect::<String>(),
                    url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>()
                )
            }
        })
        .collect::<Vec<_>>()
        .join("&");

    if let Some(mut new_pd) = post_data_create()
        && let Some(mut el) = post_data_element_create()
    {
        let bytes = new_body.as_bytes();
        el.set_to_bytes(bytes.len(), bytes.as_ptr());
        new_pd.add_element(Some(&mut el));
        req.set_post_data(Some(&mut new_pd));
    }
}

enum ProcessResult {
    Modified(Vec<u8>),
    Passthrough,
    Error,
}

/// The client_id follows the refresh_token. A response carrying a NEW
/// refresh_token is bound to the client_id of the exchange that minted it
/// (`exchange_client_id`, this request's POST). A response with no new
/// refresh_token reuses the prior one, so it keeps that generation's client_id
/// (`prior_client_id`). This mirrors the SDK, which binds each token to its
/// context's client_id and never refreshes across contexts - notably the
/// app-level `client_credentials` token (no refresh_token) must not overwrite
/// the user client_id.
fn resolve_generation_client_id(
    has_new_refresh_token: bool,
    exchange_client_id: Option<&str>,
    prior_client_id: Option<&str>,
) -> String {
    let exchange = exchange_client_id.filter(|s| !s.is_empty());
    let prior = prior_client_id.filter(|s| !s.is_empty());
    if has_new_refresh_token {
        exchange.or(prior)
    } else {
        prior.or(exchange)
    }
    .unwrap_or("")
    .to_string()
}

fn process_token_response(body: &[u8], exchange_client_id: Option<&str>) -> ProcessResult {
    process_token_response_with(body, exchange_client_id, generate_opaque)
}

/// `exchange_client_id` is the client_id from the POST that produced this
/// response (bound per request); `opaque` is the opaque-nonce generator,
/// injected for testing. When it fails (RNG unavailable) with a real token
/// present, we drop the response rather than emit the real token.
fn process_token_response_with(
    body: &[u8],
    exchange_client_id: Option<&str>,
    opaque: impl Fn() -> Option<String>,
) -> ProcessResult {
    let Ok(json_str) = std::str::from_utf8(body) else {
        return ProcessResult::Passthrough;
    };
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return ProcessResult::Passthrough;
    };
    let Some(obj) = json.as_object_mut() else {
        return ProcessResult::Passthrough;
    };

    let access_token = obj
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
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

    let at = match access_token.as_deref() {
        Some(t) if !t.is_empty() => t,
        _ => return ProcessResult::Passthrough,
    };

    // A real token is present: if opaque generation fails, drop the response
    // (Error -> FilterOutcome::Drop) instead of emitting the real token.
    let opaque_at = match opaque() {
        Some(o) => o,
        None => return ProcessResult::Error,
    };
    let opaque_rt_new = if refresh_token.is_some() {
        match opaque() {
            Some(o) => Some(o),
            None => return ProcessResult::Error,
        }
    } else {
        None
    };

    let granted_scopes: Vec<String> = scope
        .as_deref()
        .map(|s| s.split(' ').map(|s| s.to_string()).collect())
        .unwrap_or_default();

    crate::app_state::with_state(|state| {
        let now_secs = now_unix_secs();

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

        let prior_client_id = state
            .token_state
            .as_ref()
            .map(|ts| ts.current.client_id.clone());
        let client_id = resolve_generation_client_id(
            refresh_token.is_some(),
            exchange_client_id,
            prior_client_id.as_deref(),
        );

        let new_gen = crate::platform::secure_store::TokenGeneration {
            access_token: at.to_string(),
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
            client_id,
        };

        let previous = state.token_state.as_ref().map(|ts| ts.current.clone());
        state.token_state = Some(crate::platform::secure_store::StoredTokenState {
            current: new_gen,
            previous,
            previous_valid_until: Some(now_secs + 30),
        });

        state.captured_token = at.to_string();

        let data_dir = crate::state::cache_data_dir();
        if let Some(ref ts) = state.token_state {
            let _ = crate::platform::secure_store::save(&data_dir, ts);
        }
    });

    crate::ipc::plugin::scrub_pkce_verifier();

    crate::vprintln!(
        "[AUTH]   ResponseFilter captured token ({} chars)",
        at.len()
    );

    obj.insert(
        "access_token".to_string(),
        serde_json::Value::String(opaque_at),
    );
    if refresh_token.is_some() {
        let ort = crate::app_state::with_state(|state| {
            state
                .token_state
                .as_ref()
                .map(|ts| ts.current.opaque_rt.clone())
        })
        .flatten()
        .unwrap_or_default();
        obj.insert("refresh_token".to_string(), serde_json::Value::String(ort));
    }

    match serde_json::to_vec(&json) {
        Ok(v) => ProcessResult::Modified(v),
        Err(_) => ProcessResult::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_is_none_on_entropy_failure() {
        assert_eq!(generate_opaque_with(|_| false), None);
    }

    #[test]
    fn opaque_has_prefix_and_hex_bytes() {
        let o = generate_opaque_with(|buf| {
            buf.fill(0xab);
            true
        })
        .expect("RNG ok");
        assert!(o.starts_with(OPAQUE_PREFIX));
        let hex = &o[OPAQUE_PREFIX.len()..];
        assert_eq!(hex.len(), 32);
        assert_eq!(hex, "ab".repeat(16));
    }

    #[test]
    fn auto_injection_and_rewrite_share_the_api_host_set() {
        // The 5 API hosts must be recognised by both predicates (they delegate to
        // nav::is_tidal_api_host - pins the consolidation against drift).
        for url in [
            "https://api.tidal.com/v1/x",
            "https://api.tidalhifi.com/v1/x",
            "https://listen.tidal.com/x",
            "https://desktop.tidal.com/x",
            "https://openapi.tidal.com/x",
        ] {
            assert!(needs_auto_injection(url), "auto-inject: {url}");
            assert!(should_rewrite_token(url), "rewrite: {url}");
        }
        // auth/login are rewrite-only (not auto-injected).
        assert!(should_rewrite_token("https://auth.tidal.com/oauth2/token"));
        assert!(!needs_auto_injection("https://auth.tidal.com/oauth2/token"));
        // http scheme is rejected by both.
        assert!(!needs_auto_injection("http://api.tidal.com/x"));
        assert!(!should_rewrite_token("http://api.tidal.com/x"));
    }

    #[test]
    fn token_response_drops_on_entropy_failure_never_passthrough() {
        // A real-token response with opaque generation failing must DROP
        // (Error), never Passthrough, or the real token reaches the renderer.
        let body = br#"{"access_token":"real-secret","refresh_token":"real-rt"}"#;
        assert!(matches!(
            process_token_response_with(body, None, || None),
            ProcessResult::Error
        ));
    }

    #[test]
    fn new_refresh_token_binds_this_exchange_client_id_not_prior() {
        // The bug: authorization_code (new refresh_token, client_id "user")
        // arriving after a client_credentials gen (client_id "app") must adopt
        // "user", not stay stuck on the prior "app".
        assert_eq!(
            resolve_generation_client_id(true, Some("user"), Some("app")),
            "user"
        );
    }

    #[test]
    fn no_new_refresh_token_keeps_prior_client_id() {
        // client_credentials (no refresh_token) must not overwrite the user
        // client_id already held.
        assert_eq!(
            resolve_generation_client_id(false, Some("app"), Some("user")),
            "user"
        );
    }

    #[test]
    fn rotating_refresh_keeps_same_client_id() {
        // A refresh that rotates the token stays on the same client.
        assert_eq!(
            resolve_generation_client_id(true, Some("user"), Some("user")),
            "user"
        );
    }

    #[test]
    fn empty_exchange_falls_back_to_prior_on_new_refresh() {
        // Defensive: a new refresh_token with no observed client_id keeps the
        // prior rather than blanking it.
        assert_eq!(
            resolve_generation_client_id(true, None, Some("user")),
            "user"
        );
        assert_eq!(
            resolve_generation_client_id(true, Some(""), Some("user")),
            "user"
        );
    }

    #[test]
    fn first_gen_with_no_prior_takes_exchange() {
        // First persist after a session clear: no prior, take the exchange's.
        assert_eq!(
            resolve_generation_client_id(true, Some("user"), None),
            "user"
        );
        assert_eq!(
            resolve_generation_client_id(false, Some("app"), None),
            "app"
        );
    }

    fn generation(tag: &str) -> crate::platform::secure_store::TokenGeneration {
        crate::platform::secure_store::TokenGeneration {
            access_token: format!("real_at_{tag}"),
            refresh_token: format!("real_rt_{tag}"),
            opaque_at: format!("luna_at_{tag}"),
            opaque_rt: format!("luna_rt_{tag}"),
            version: 1,
            access_expires: u64::MAX,
            user_id: None,
            granted_scopes: vec![],
            client_id: "cid".into(),
        }
    }

    fn stored(
        current: crate::platform::secure_store::TokenGeneration,
        previous: Option<crate::platform::secure_store::TokenGeneration>,
        previous_valid_until: Option<u64>,
    ) -> crate::platform::secure_store::StoredTokenState {
        crate::platform::secure_store::StoredTokenState {
            current,
            previous,
            previous_valid_until,
        }
    }

    #[test]
    fn resolve_access_current_and_previous_within_window() {
        let ts = stored(generation("new"), Some(generation("old")), Some(100));
        // current opaque -> current real
        assert_eq!(resolve_opaque_access(&ts, "luna_at_new", 50), "real_at_new");
        // previous opaque, still in window -> previous real
        assert_eq!(resolve_opaque_access(&ts, "luna_at_old", 50), "real_at_old");
    }

    #[test]
    fn resolve_access_out_of_window_previous_falls_back_to_current() {
        let ts = stored(generation("new"), Some(generation("old")), Some(100));
        // previous opaque, past the window -> current (not left raw)
        assert_eq!(
            resolve_opaque_access(&ts, "luna_at_old", 200),
            "real_at_new"
        );
    }

    #[test]
    fn resolve_refresh_unknown_opaque_falls_back_to_current() {
        // The ~1h bug: an opaque_rt the SDK held past our window must resolve to
        // the current real refresh token, never leave raw.
        let ts = stored(generation("new"), Some(generation("old")), Some(100));
        assert_eq!(
            resolve_opaque_refresh(&ts, "luna_rt_ancient", 50),
            "real_rt_new"
        );
        // previous rt within window still maps to the matching real rt
        assert_eq!(
            resolve_opaque_refresh(&ts, "luna_rt_old", 50),
            "real_rt_old"
        );
        // and to current once out of window
        assert_eq!(
            resolve_opaque_refresh(&ts, "luna_rt_old", 200),
            "real_rt_new"
        );
    }

    #[test]
    fn resolve_no_previous_maps_everything_to_current() {
        let ts = stored(generation("new"), None, None);
        assert_eq!(resolve_opaque_access(&ts, "luna_at_new", 0), "real_at_new");
        assert_eq!(
            resolve_opaque_access(&ts, "luna_at_whatever", 0),
            "real_at_new"
        );
        assert_eq!(
            resolve_opaque_refresh(&ts, "luna_rt_whatever", 0),
            "real_rt_new"
        );
    }
}
