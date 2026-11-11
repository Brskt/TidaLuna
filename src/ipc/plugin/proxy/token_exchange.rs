use crate::app_state::with_state;

pub(super) fn proxy_rewrite_refresh_body(body: &str) -> String {
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

    let now = crate::util::now_unix_secs();
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

pub(super) fn proxy_transform_token_body(body: &str, status: u16, session_epoch: u64) -> String {
    proxy_transform_token_body_with(
        body,
        status,
        session_epoch,
        crate::ui::token_filter::generate_opaque,
    )
}

/// `opaque` is the opaque-nonce generator, injected for testing. When it fails
/// (RNG unavailable) with a real token present, return an empty token body
/// rather than the real token (the recipient is plugin JS).
///
/// `session_epoch` names the session the fetch was issued under. The reply can land after
/// a logout, and committing it would hand the departed account's bearer token back to the
/// live process.
fn proxy_transform_token_body_with(
    body: &str,
    status: u16,
    session_epoch: u64,
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

    // `None` is the refusal, not an empty nonce: the check has to sit inside the closure to
    // stay atomic with the writes it guards. The body built after this call goes back to the
    // plugin; the refusal has to travel out with it.
    let stored_opaque_rt = with_state(|state| {
        // The session that issued this exchange may have ended while it was on the network.
        // Nothing below may run then: `captured_token` is what the proxy injects as
        // `Authorization: Bearer`. Committing would re-authenticate the live process as
        // the departed account. The bump and this read share the one `AppState` mutex, which
        // orders them. The plugin still receives an opaque token; it resolves to nothing,
        // which is the correct answer for a session that is over.
        if state.session_epoch != session_epoch {
            crate::vprintln!("[AUTH]   Exchange answered for a session already left, dropped");
            return None;
        }
        let now_secs = crate::util::now_unix_secs();

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
        Some(ort)
    })
    .flatten();

    // Nothing registered means the opaque this would hand the plugin resolves to whichever
    // session is current instead of to this exchange. Same fail-closed answer the RNG failure
    // above gives, for the same reason: an empty body beats a credential that names the
    // wrong account.
    let Some(stored_opaque_rt) = stored_opaque_rt else {
        return "{}".to_string();
    };

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

#[cfg(test)]
#[path = "../../../../tests/unit/ipc/plugin/proxy/token_exchange.rs"]
mod tests;
