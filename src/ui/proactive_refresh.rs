//! Proactive token refresh: replaces real JWTs in JS memory with opaques
//! seconds after boot instead of waiting for TIDAL's SDK refresh near token expiry.
//!
//! Called from the web.loaded IPC handler when needs_proactive_refresh is set.

pub(crate) fn trigger_if_needed() {
    let needs = crate::app_state::with_state(|state| {
        let needs = state.needs_proactive_refresh;
        state.needs_proactive_refresh = false;
        needs
    })
    .unwrap_or(false);

    if !needs {
        return;
    }

    let (refresh_token, client_id, scope) = match crate::app_state::with_state(|state| {
        let ts = state.token_state.as_ref()?;
        let cid = if ts.current.client_id.is_empty() {
            state.last_client_id.clone()
        } else {
            ts.current.client_id.clone()
        };
        Some((
            ts.current.refresh_token.clone(),
            cid,
            ts.current.granted_scopes.join(" "),
        ))
    })
    .flatten()
    {
        Some(triple) => triple,
        None => return,
    };

    if refresh_token.is_empty() {
        return;
    }

    crate::vprintln!("[AUTH]   Proactive refresh: starting");
    crate::state::rt_handle().spawn(async move {
        do_refresh(refresh_token, client_id, scope).await;
        // Open the gate on every exit path; safe even on refresh failure (the
        // token stays live, but the egress filter blocks exfil).
        crate::ipc::plugin::open_plugin_gate();
    });
}

async fn do_refresh(refresh_token: String, client_id: String, req_scope: String) {
    if client_id.is_empty() {
        // The token endpoint rejects a refresh grant with no client_id (400,
        // sub_status 1002): a request here can only fail. Skip it; the token
        // stays live and the SDK refreshes it near expiry.
        crate::vprintln!("[AUTH]   Proactive refresh: no client_id - skipped");
        return;
    }

    let client = &*crate::state::HTTP_CLIENT;

    let mut body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        url::form_urlencoded::byte_serialize(refresh_token.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(client_id.as_bytes()).collect::<String>(),
    );
    if !req_scope.is_empty() {
        body.push_str("&scope=");
        body.push_str(
            &url::form_urlencoded::byte_serialize(req_scope.as_bytes()).collect::<String>(),
        );
    }

    let url = format!(
        "https://{}{}",
        crate::ui::nav::HOST_AUTH,
        crate::ui::nav::PATH_OAUTH_TOKEN
    );

    let resp = match client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            crate::vprintln!("[AUTH]   Proactive refresh failed: {e}");
            return;
        }
    };

    let status = resp.status().as_u16();
    let resp_body = resp.text().await.unwrap_or_default();

    if status != 200 {
        crate::vprintln!(
            "[AUTH]   Proactive refresh: {} {}",
            status,
            crate::util::truncate_str(&resp_body, 200)
        );
        return;
    }

    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&resp_body) else {
        crate::vprintln!("[AUTH]   Proactive refresh: invalid JSON");
        return;
    };
    let Some(obj) = json.as_object_mut() else {
        return;
    };

    let at = match obj.get("access_token").and_then(|v| v.as_str()) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => {
            crate::vprintln!("[AUTH]   Proactive refresh: no access_token in response");
            return;
        }
    };

    let new_rt = obj
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

    let opaque_at = match crate::ui::token_filter::generate_opaque() {
        Some(o) => o,
        None => return,
    };
    // Opaque for the refresh token, generated off the AppState lock (only when a
    // new refresh token is present). Skip this refresh cycle on RNG failure.
    let opaque_rt_new = if new_rt.is_some() {
        match crate::ui::token_filter::generate_opaque() {
            Some(o) => Some(o),
            None => return,
        }
    } else {
        None
    };
    let granted_scopes: Vec<String> = scope
        .as_deref()
        .map(|s| s.split(' ').map(|s| s.to_string()).collect())
        .unwrap_or_default();

    // Build session JS before taking the lock (avoid holding it during string ops)
    let session_user_id = user_id.clone().unwrap_or_default();
    let session_scopes_json =
        serde_json::to_string(&granted_scopes).unwrap_or_else(|_| "[]".to_string());

    crate::app_state::with_state(|state| {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let (real_rt, ort) = if let Some(ref rt) = new_rt {
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
            opaque_rt: ort,
            version: state
                .token_state
                .as_ref()
                .map(|ts| ts.current.version + 1)
                .unwrap_or(1),
            access_expires: now_secs + expires_in.unwrap_or(3600),
            user_id: user_id.clone(),
            granted_scopes: granted_scopes.clone(),
            // Backfill from the client_id this refresh used; a blob that
            // arrived without one self-heals instead of staying empty.
            client_id: state
                .token_state
                .as_ref()
                .map(|ts| ts.current.client_id.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| client_id.clone()),
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
            // Same rule as the two token paths, and this is the site that meets
            // it most easily. The refresh awaits the network, a session_clear can
            // empty token_state in the gap, and a refresh that reuses its token
            // returns no new one. Both arms of the fallback are then gone and the
            // generation would carry no refresh token at all.
            if ts.current.is_durable() {
                if let Err(e) = crate::platform::secure_store::save(&data_dir, ts) {
                    crate::vprintln!("[AUTH]   Failed to persist token state: {e:?}");
                }
            } else {
                crate::vprintln!(
                    "[AUTH]   Refresh landed with no refresh token - stored credential kept"
                );
            }
        }
    });

    // Push opaques to TIDAL's SDK via session delegate
    let expires_ms = crate::app_state::with_state(|state| {
        state
            .token_state
            .as_ref()
            .map(|ts| ts.current.access_expires * 1000)
    })
    .flatten()
    .unwrap_or(0);

    let js = format!(
        "if(window.nativeInterface&&window.nativeInterface.userSession){{window.nativeInterface.userSession.update({{token:{at},expires:{expires},userId:{uid},grantedScopes:{scopes},clientId:{cid}}})}}",
        at = serde_json::to_string(&opaque_at).unwrap_or_default(),
        expires = expires_ms,
        uid = serde_json::to_string(&session_user_id).unwrap_or_default(),
        scopes = session_scopes_json,
        cid = serde_json::to_string(&client_id).unwrap_or_default(),
    );
    crate::app_state::eval_js(&js);

    crate::vprintln!(
        "[AUTH]   Proactive refresh complete ({} chars) - opaques pushed to SDK",
        at.len()
    );
}
