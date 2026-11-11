use cef::*;
use std::sync::Mutex;

use super::{is_opaque, userfree_to_string};
use crate::ui::nav::RequestUrl;
use crate::util::now_unix_secs;

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

pub(super) fn capture_client_id(req: &mut Request, exchange_slot: &Mutex<Option<String>>) {
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
/// must have checked `is_opaque`; callers gate this on `token_state` = Some:
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

pub(super) fn rewrite_authorization_header(req: &mut Request) {
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
    // resolves to a real token; nothing opaque reaches the wire.
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

pub(super) fn inject_refresh_token(req: &mut Request, url: &RequestUrl) {
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
    // resolves to the current real refresh token; the SDK's own refresh
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

#[cfg(test)]
#[path = "../../../tests/unit/ui/token_filter/request.rs"]
mod tests;
