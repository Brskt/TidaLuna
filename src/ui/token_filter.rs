use cef::*;
use std::cell::RefCell;

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
/// Subset of should_rewrite_token — excludes telemetry/DRM hosts that don't need OAuth.
pub(crate) fn needs_auto_injection(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    let host = parsed.host_str().unwrap_or("");
    matches!(
        host,
        "api.tidal.com"
            | "api.tidalhifi.com"
            | "listen.tidal.com"
            | "desktop.tidal.com"
            | "openapi.tidal.com"
    )
}

pub(crate) fn should_rewrite_token(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    let host = parsed.host_str().unwrap_or("");
    matches!(
        host,
        "api.tidal.com"
            | "api.tidalhifi.com"
            | "listen.tidal.com"
            | "desktop.tidal.com"
            | "openapi.tidal.com"
            | "login.tidal.com"
            | "auth.tidal.com"
    ) || (host == "fp.fa.tidal.com" && parsed.path().starts_with("/license"))
        || (host.starts_with("event-collector.") && host.ends_with(".tidalhi.fi"))
}

pub(crate) fn generate_opaque() -> String {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).expect("getrandom failed");
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!("{OPAQUE_PREFIX}{hex}")
}

pub(crate) fn is_opaque(value: &str) -> bool {
    value.starts_with(OPAQUE_PREFIX)
}

// Unconditionally cancels the request. Used by the exfiltration guard
// to block sendBeacon (RT_PING) and WebSocket upgrades to non-Tidal domains.
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
    pub(super) struct TokenResourceHandler;

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
                    capture_client_id(req);
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
                Some(TokenResponseFilter::new(RefCell::new(
                    FilterState::Accumulating(Vec::new()),
                )))
            } else {
                None
            }
        }
    }
}

fn capture_client_id(req: &mut Request) {
    let method_cef = req.method();
    let method = userfree_to_string(&method_cef);
    if method != "POST" {
        return;
    }
    let Some(post_data) = req.post_data() else {
        return;
    };
    let count = post_data.element_count();
    if count == 0 {
        return;
    }
    let mut elements: Vec<Option<PostDataElement>> = vec![None; count];
    post_data.elements(Some(&mut elements));
    let body_bytes = match elements.first() {
        Some(Some(el)) => {
            let count = el.bytes_count();
            if count == 0 {
                return;
            }
            let mut buf = vec![0u8; count];
            el.bytes(count, buf.as_mut_ptr());
            buf
        }
        _ => return,
    };
    let Ok(body_str) = std::str::from_utf8(&body_bytes) else {
        return;
    };
    for (k, v) in url::form_urlencoded::parse(body_str.as_bytes()) {
        if k == "client_id" && !v.is_empty() {
            crate::app_state::with_state(|state| {
                state.last_client_id = v.to_string();
            });
            return;
        }
    }
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

    let real_token = crate::app_state::with_state(|state| {
        let ts = state.token_state.as_ref()?;
        if opaque == ts.current.opaque_at {
            return Some(ts.current.access_token.clone());
        }
        if let Some(ref prev) = ts.previous {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if let Some(valid_until) = ts.previous_valid_until
                && now > valid_until
            {
                return None;
            }
            if opaque == prev.opaque_at {
                return Some(prev.access_token.clone());
            }
        }
        None
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
    let method_cef = req.method();
    let method = userfree_to_string(&method_cef);
    if method != "POST" {
        return;
    }

    let Some(post_data) = req.post_data() else {
        return;
    };
    let count = post_data.element_count();
    if count == 0 {
        return;
    }
    let mut elements: Vec<Option<PostDataElement>> = vec![None; count];
    post_data.elements(Some(&mut elements));

    let body_bytes = match elements.first() {
        Some(Some(el)) => {
            let count = el.bytes_count();
            if count == 0 {
                return;
            }
            let mut buf = vec![0u8; count];
            el.bytes(count, buf.as_mut_ptr());
            buf
        }
        _ => return,
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

    let real_rt = crate::app_state::with_state(|state| {
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

#[derive(Clone)]
enum FilterState {
    Accumulating(Vec<u8>),
    Emitting { data: Vec<u8>, offset: usize },
    Done,
    Error,
}

wrap_response_filter! {
    pub(super) struct TokenResponseFilter {
        state: RefCell<FilterState>,
    }

    impl ResponseFilter {
        fn init_filter(&self) -> ::std::os::raw::c_int {
            1
        }

        fn filter(
            &self,
            data_in: Option<&mut Vec<u8>>,
            data_in_read: Option<&mut usize>,
            data_out: Option<&mut Vec<u8>>,
            data_out_written: Option<&mut usize>,
        ) -> ResponseFilterStatus {
            let mut state = self.state.borrow_mut();
            let out_written = match data_out_written {
                Some(w) => w,
                None => return ResponseFilterStatus::ERROR,
            };
            *out_written = 0;

            match &mut *state {
                FilterState::Accumulating(buf) => {
                    if let Some(input) = data_in {
                        if let Some(read) = data_in_read {
                            *read = input.len();
                        }
                        buf.extend_from_slice(input);
                        ResponseFilterStatus::NEED_MORE_DATA
                    } else {
                        let accumulated = std::mem::take(buf);
                        match process_token_response(&accumulated) {
                            ProcessResult::Modified(modified) => {
                                *state = FilterState::Emitting {
                                    data: modified,
                                    offset: 0,
                                };
                                drop(state);
                                self.emit(data_out, out_written)
                            }
                            ProcessResult::Passthrough => {
                                *state = FilterState::Emitting {
                                    data: accumulated,
                                    offset: 0,
                                };
                                drop(state);
                                self.emit(data_out, out_written)
                            }
                            ProcessResult::Error => {
                                *state = FilterState::Error;
                                ResponseFilterStatus::ERROR
                            }
                        }
                    }
                }
                FilterState::Emitting { .. } => {
                    if let Some(input) = data_in
                        && let Some(read) = data_in_read
                    {
                        *read = input.len();
                    }
                    drop(state);
                    self.emit(data_out, out_written)
                }
                FilterState::Done => ResponseFilterStatus::DONE,
                FilterState::Error => ResponseFilterStatus::ERROR,
            }
        }
    }
}

impl TokenResponseFilter {
    fn emit(
        &self,
        data_out: Option<&mut Vec<u8>>,
        out_written: &mut usize,
    ) -> ResponseFilterStatus {
        let mut state = self.state.borrow_mut();
        let (data, offset) = match &mut *state {
            FilterState::Emitting { data, offset } => (data, offset),
            _ => return ResponseFilterStatus::ERROR,
        };

        let remaining = &data[*offset..];
        if remaining.is_empty() {
            *state = FilterState::Done;
            return ResponseFilterStatus::DONE;
        }

        let Some(out_buf) = data_out else {
            return ResponseFilterStatus::NEED_MORE_DATA;
        };
        let to_write = remaining.len().min(out_buf.len());
        out_buf[..to_write].copy_from_slice(&remaining[..to_write]);
        *out_written = to_write;
        *offset += to_write;

        if *offset >= data.len() {
            *state = FilterState::Done;
            ResponseFilterStatus::DONE
        } else {
            ResponseFilterStatus::NEED_MORE_DATA
        }
    }
}

enum ProcessResult {
    Modified(Vec<u8>),
    Passthrough,
    Error,
}

fn process_token_response(body: &[u8]) -> ProcessResult {
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

    let opaque_at = generate_opaque();

    let granted_scopes: Vec<String> = scope
        .as_deref()
        .map(|s| s.split(' ').map(|s| s.to_string()).collect())
        .unwrap_or_default();

    crate::app_state::with_state(|state| {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let (real_rt, ort) = if let Some(ref rt) = refresh_token {
            (rt.clone(), generate_opaque())
        } else if let Some(ref ts) = state.token_state {
            (
                ts.current.refresh_token.clone(),
                ts.current.opaque_rt.clone(),
            )
        } else {
            (String::new(), String::new())
        };

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

        state.captured_token = at.to_string();

        let data_dir = crate::state::cache_data_dir();
        if let Some(ref ts) = state.token_state {
            let _ = crate::platform::secure_store::save(&data_dir, ts);
        }
    });

    crate::ipc::plugin::scrub_pkce_verifier();

    let masked = &at[..at.len().min(12)];
    crate::vprintln!(
        "[AUTH]   ResponseFilter captured token ({}... {} chars)",
        masked,
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
