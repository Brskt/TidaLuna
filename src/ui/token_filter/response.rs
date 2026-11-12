use super::generate_opaque;
use crate::app_state::{TokenCommit, TokenGrant};

pub(super) enum ProcessResult {
    Modified(Vec<u8>),
    Passthrough,
    Error,
}

/// The client_id follows the refresh_token. A response carrying a NEW
/// refresh_token is bound to the client_id of the exchange that minted it
/// (`exchange_client_id`, this request's POST). A response with no new
/// refresh_token reuses the prior one: it keeps that generation's client_id
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

pub(super) fn process_token_response(
    body: &[u8],
    exchange_client_id: Option<&str>,
    session_epoch: u64,
) -> ProcessResult {
    process_token_response_with(body, exchange_client_id, session_epoch, generate_opaque)
}

/// `exchange_client_id` is the client_id from the POST that produced this
/// response (bound per request); `opaque` is the opaque-nonce generator,
/// injected for testing. When it fails (RNG unavailable) with a real token
/// present, we drop the response rather than emit the real token.
///
/// `session_epoch` is the session the POST was issued under. A logout can land between
/// the request and its reply, and committing then would put the departed account's live
/// bearer token back into `AppState`.
fn process_token_response_with(
    body: &[u8],
    exchange_client_id: Option<&str>,
    session_epoch: u64,
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

    let grant = TokenGrant {
        access_token: at,
        refresh_token: refresh_token.as_deref(),
        opaque_at: &opaque_at,
        opaque_rt: opaque_rt_new.as_deref(),
        expires_in,
        user_id: user_id.as_deref(),
        granted_scopes: &granted_scopes,
    };
    // The check belongs inside the closure, atomic with the writes it guards; its answer has
    // to leave it, because the body this function goes on to build is delivered to the SDK.
    let committed = crate::app_state::with_state(|state| {
        let commit = state.commit_token_generation(session_epoch, &grant, |state| {
            resolve_generation_client_id(
                refresh_token.is_some(),
                exchange_client_id,
                state
                    .token_state
                    .as_ref()
                    .map(|ts| ts.current.client_id.as_str()),
            )
        });
        match commit {
            TokenCommit::Refused => {
                crate::vprintln!(
                    "[AUTH]   Token response answered for a session already left, dropped"
                );
                false
            }
            TokenCommit::Committed { durable, .. } => {
                if !durable {
                    crate::vprintln!(
                        "[AUTH]   Exchange carried no refresh token - stored credential kept"
                    );
                }
                true
            }
        }
    })
    .unwrap_or(false);

    // Nothing registered means the opaque below would resolve to whichever session is
    // current rather than to this response's token. Dropping the body fails this one
    // exchange visibly instead of handing the SDK a credential for somebody else's session;
    // an exchange that still belongs to the live session carries a matching epoch and never
    // reaches here.
    if !committed {
        return ProcessResult::Error;
    }

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
#[path = "../../../tests/unit/ui/token_filter/response.rs"]
mod tests;
