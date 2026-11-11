use crate::app_state::{AppState, with_state};

/// Replacement for a real token with no opaque mapping (a captured token seen before
/// token_state exists). Must not start with `luna_`: `is_opaque()` would otherwise accept it.
const REDACTED_MARKER: &str = "[redacted-token]";

/// Real OAuth tokens in play paired with the opaque they map to. Single source for
/// `scrub_real_tokens` (uses both halves) and `leaks_real_token` (uses the real).
pub(super) fn real_token_pairs(state: &AppState) -> Vec<(String, String)> {
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
pub(in crate::ipc::plugin) fn leaks_real_token(url: &str, payload: Option<&str>) -> bool {
    with_state(|state| {
        real_token_pairs(state).iter().any(|(real, _)| {
            !real.is_empty()
                && (url.contains(real.as_str())
                    || payload.is_some_and(|p| p.contains(real.as_str())))
        })
    })
    .unwrap_or(false)
}

/// Replace any real OAuth token present in `text` with its opaque counterpart.
/// `pairs` is (real_token, replacement), gathered from the token state.
pub(super) fn scrub_real_tokens_with(text: String, pairs: &[(String, String)]) -> String {
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
#[path = "../../../../tests/unit/ipc/plugin/proxy/scrub.rs"]
mod tests;
