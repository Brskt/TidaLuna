use super::{Answer, ExchangeOrigin, generate_opaque};
use crate::app_state::{GrantKind, TokenCommit, TokenGrant};

pub(super) enum ProcessResult {
    Modified(Vec<u8>),
    Passthrough,
    Error,
}

pub(super) fn process_token_response(
    body: &[u8],
    origin: ExchangeOrigin,
    exchange_client_id: Option<&str>,
    session_epoch: u64,
) -> ProcessResult {
    process_token_response_with(
        body,
        origin,
        exchange_client_id,
        session_epoch,
        generate_opaque,
    )
}

/// Whether an answer would hand the page a token: a non-empty `access_token` or `refresh_token`.
fn carries_a_token(obj: &serde_json::Map<String, serde_json::Value>) -> bool {
    ["access_token", "refresh_token"].iter().any(|key| {
        obj.get(*key)
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.is_empty())
    })
}

/// `exchange_client_id` is the client_id from the POST that produced this
/// response (bound per request); `opaque` is the opaque-nonce generator,
/// injected for testing. When it fails (RNG unavailable) with a real token
/// present, we drop the response rather than emit the real token.
///
/// `session_epoch` is the session the POST was issued under. A logout can land between
/// the request and its reply, and committing then would put the departed account's live
/// bearer token back into `AppState`.
///
/// The body is decoded the way `Response.json()` decodes it, replacement characters and a leading
/// BOM dropped: a body the page can parse is a body this reads. An `Inert` answer carrying a token
/// is dropped; its error bodies pass.
fn process_token_response_with(
    body: &[u8],
    origin: ExchangeOrigin,
    exchange_client_id: Option<&str>,
    session_epoch: u64,
    opaque: impl Fn() -> Option<String>,
) -> ProcessResult {
    let decoded = String::from_utf8_lossy(body);
    let json_str = decoded.strip_prefix('\u{FEFF}').unwrap_or(&decoded);
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return ProcessResult::Passthrough;
    };
    let Some(obj) = json.as_object_mut() else {
        return ProcessResult::Passthrough;
    };
    if origin == ExchangeOrigin::Inert {
        return if carries_a_token(obj) {
            ProcessResult::Error
        } else {
            ProcessResult::Passthrough
        };
    }

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
    let kind = GrantKind::of(
        refresh_token.as_deref().is_some_and(|rt| !rt.is_empty()),
        user_id.is_some(),
    );
    let answer = origin.answer(kind);
    if answer == Answer::Drop {
        crate::vprintln!("[AUTH]   {kind:?} answer to a {origin:?} request, dropped");
        return ProcessResult::Error;
    }
    let granted_scopes: Vec<String> = scope
        .as_deref()
        .map(|s| s.split(' ').map(|s| s.to_string()).collect())
        .unwrap_or_default();

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

    let grant = TokenGrant {
        access_token: at,
        refresh_token: refresh_token.as_deref(),
        opaque_at: &opaque_at,
        opaque_rt: opaque_rt_new.as_deref(),
        expires_in,
        user_id: user_id.as_deref(),
        granted_scopes: &granted_scopes,
        client_id: exchange_client_id,
    };
    let rule = match answer {
        Answer::User(rule) => rule,
        Answer::Application => {
            // Its own nonce, stable: the user's generation is not touched and nothing is scrubbed.
            let Some(nonce) = crate::app_state::with_state(|state| state.apply_app_token(&grant))
            else {
                return ProcessResult::Error;
            };
            obj.insert("access_token".to_string(), serde_json::Value::String(nonce));
            return match serde_json::to_vec(&json) {
                Ok(v) => ProcessResult::Modified(v),
                Err(_) => ProcessResult::Error,
            };
        }
        Answer::Drop | Answer::SameUserInPlace => return ProcessResult::Error,
    };
    // The check belongs inside the closure, atomic with the writes it guards; its answer has
    // to leave it, because the body this function goes on to build is delivered to the SDK.
    let committed = crate::app_state::with_state(|state| {
        match state.commit_token_generation(session_epoch, &grant, rule) {
            TokenCommit::Refused => {
                crate::vprintln!(
                    "[AUTH]   Token response answered for a session already left, dropped"
                );
                None
            }
            TokenCommit::SwappedAccount => {
                crate::verr!("[AUTH] refresh answered for another account, dropped");
                None
            }
            TokenCommit::Committed { durable, ended } => {
                if !durable {
                    crate::vprintln!(
                        "[AUTH]   Exchange carried no refresh token - stored credential kept"
                    );
                }
                Some(ended)
            }
        }
    })
    .flatten();

    // Nothing registered means the opaque below would resolve to whichever session is
    // current rather than to this response's token. Dropping the body fails this one
    // exchange visibly instead of handing the SDK a credential for somebody else's session;
    // an exchange that still belongs to the live session carries a matching epoch and never
    // reaches here.
    let Some(ended) = committed else {
        return ProcessResult::Error;
    };
    // A sign-in to another account closed the session before it: its plugins go, off the lock.
    if let Some(ended) = ended {
        crate::ipc::plugin::settle_ended_session(&ended);
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
